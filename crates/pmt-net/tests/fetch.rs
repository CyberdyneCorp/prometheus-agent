//! In-process fetch SSRF protection and platform TLS verification against
//! local servers (spec: security-boundaries, Private and metadata
//! destinations, In-process fetch SSRF protection, Platform trust store for
//! TLS). `127.0.0.1` stands for a public host through
//! `security.egress.fake_ip_ranges`, the way a TUN proxy's answers do.

use std::collections::HashMap;
use std::io;
use std::net::IpAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};

use pmt_net::fetch::MAX_REDIRECTS;
use pmt_net::resolve::{Lookup, LookupFuture};
use pmt_net::{DestinationPolicy, Fetcher, OutboundPolicy, PrivateGrant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;

/// A resolver with fixed answers; other names fail to resolve.
struct Names(HashMap<&'static str, Vec<IpAddr>>);

impl Lookup for Names {
    fn lookup(&self, host: &str) -> LookupFuture {
        let answer = self
            .0
            .get(host)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, host.to_owned()));
        Box::pin(async move { answer })
    }
}

fn names() -> Arc<Names> {
    let ip = |s: &str| s.parse::<IpAddr>().unwrap();
    Arc::new(Names(HashMap::from([
        ("public.test", vec![ip("127.0.0.1")]),
        ("corp.test", vec![ip("127.0.0.1")]),
        ("metadata.test", vec![ip("169.254.169.254")]),
        ("internal.test", vec![ip("10.0.0.7")]),
        ("rebind.test", vec![ip("127.0.0.1"), ip("192.168.1.1")]),
    ])))
}

/// Loopback plays the public internet.
fn public_loopback() -> DestinationPolicy {
    DestinationPolicy::new(false, &["127.0.0.1/32".into()]).unwrap()
}

fn fetcher(policy: DestinationPolicy) -> Fetcher {
    Fetcher::with_lookup(&OutboundPolicy::default(), policy, names()).unwrap()
}

/// Requested paths, in order.
type Seen = Arc<Mutex<Vec<String>>>;

/// Answers each request with `route(path)` (a full status line + headers).
async fn serve(route: fn(&str, u16) -> String) -> (u16, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen: Seen = Arc::default();
    let log = Arc::clone(&seen);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(answer(stream, port, route, Arc::clone(&log)));
        }
    });
    (port, seen)
}

async fn answer<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    port: u16,
    route: fn(&str, u16) -> String,
    seen: Seen,
) {
    let mut buf = vec![0u8; 4096];
    let n = stream.read(&mut buf).await.unwrap_or(0);
    let request = String::from_utf8_lossy(&buf[..n]);
    let path = request.split_whitespace().nth(1).unwrap_or("/").to_owned();
    seen.lock().unwrap().push(path.clone());
    let _ = stream.write_all(route(&path, port).as_bytes()).await;
    let _ = stream.shutdown().await;
}

fn redirect(location: &str) -> String {
    format!(
        "HTTP/1.1 302 Found\r\nlocation: {location}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
    )
}

fn ok(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn routes(path: &str, port: u16) -> String {
    match path {
        "/to-metadata" => redirect("http://169.254.169.254/latest/meta-data"),
        "/to-metadata-name" => redirect("http://metadata.test/latest/meta-data"),
        "/to-internal" => redirect("http://internal.test/admin"),
        "/to-file" => redirect("file:///etc/passwd"),
        "/hop" => redirect(&format!("http://public.test:{port}/landed")),
        p if p.starts_with("/loop/") => {
            let n: usize = p["/loop/".len()..].parse().unwrap();
            redirect(&format!("/loop/{}", n + 1))
        }
        _ => ok("hello"),
    }
}

fn blocked(result: Result<reqwest::Response, pmt_core::Error>) -> pmt_core::Error {
    let err = result.expect_err("fetch should be refused");
    assert_eq!(err.code().code, "net.destination_blocked", "{err}");
    err
}

#[tokio::test]
async fn redirect_to_the_metadata_endpoint_is_refused() {
    let (port, seen) = serve(routes).await;
    let fetch = fetcher(public_loopback());

    let err = blocked(
        fetch
            .get(&format!("http://public.test:{port}/to-metadata"))
            .await,
    );
    assert!(err.message().contains("cloud metadata endpoint"), "{err}");
    blocked(
        fetch
            .get(&format!("http://public.test:{port}/to-metadata-name"))
            .await,
    );
    // Only the public hop was requested; nothing went to a metadata address.
    assert_eq!(*seen.lock().unwrap(), ["/to-metadata", "/to-metadata-name"]);
}

#[tokio::test]
async fn metadata_literals_are_refused_even_with_allow_private() {
    let fetch = fetcher(DestinationPolicy::new(true, &[]).unwrap());
    for url in [
        "http://169.254.169.254/",
        "http://[::ffff:169.254.169.254]/",
        "http://[fd00:ec2::254]/",
        "http://2852039166/latest",
        "http://metadata.google.internal/computeMetadata/v1/",
    ] {
        let err = blocked(fetch.get(url).await);
        assert!(err.message().contains("metadata"), "{url}: {err}");
        assert_eq!(err.exit_code(), 77);
    }
}

#[tokio::test]
async fn every_redirect_hop_is_rechecked() {
    let (port, seen) = serve(routes).await;
    let fetch = fetcher(public_loopback());

    let body = fetch
        .get(&format!("http://public.test:{port}/hop"))
        .await
        .unwrap();
    assert_eq!(body.text().await.unwrap(), "hello");
    blocked(
        fetch
            .get(&format!("http://public.test:{port}/to-internal"))
            .await,
    );
    let err = fetch
        .get(&format!("http://public.test:{port}/to-file"))
        .await
        .unwrap_err();
    assert_eq!(err.code().code, "net.invalid_url");
    assert_eq!(
        *seen.lock().unwrap(),
        ["/hop", "/landed", "/to-internal", "/to-file"]
    );

    let err = fetch
        .get(&format!("http://public.test:{port}/loop/0"))
        .await
        .unwrap_err();
    assert_eq!(err.code().code, "net.too_many_redirects");
    assert_eq!(seen.lock().unwrap().len(), 4 + MAX_REDIRECTS + 1);
}

#[tokio::test]
async fn dns_failure_is_reported_as_blocked() {
    let err = blocked(fetcher(public_loopback()).get("http://nx.test/").await);
    assert!(err.message().contains("did not resolve"), "{err}");
}

#[tokio::test]
async fn private_destinations_need_allow_private_or_a_grant() {
    let (port, _) = serve(routes).await;
    let url = format!("http://127.0.0.1:{port}/");
    blocked(fetcher(DestinationPolicy::default()).get(&url).await);
    // One private answer among public ones refuses the host.
    blocked(
        fetcher(public_loopback())
            .get(&format!("http://rebind.test:{port}/"))
            .await,
    );

    let granted = DestinationPolicy::default()
        .with_grant(PrivateGrant::parse(&format!("127.0.0.1:{port}")).unwrap());
    assert!(
        fetcher(granted)
            .get(&url)
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let other_port =
        DestinationPolicy::default().with_grant(PrivateGrant::parse("127.0.0.1:1").unwrap());
    blocked(fetcher(other_port).get(&url).await);
    let allowed = DestinationPolicy::new(true, &[]).unwrap();
    assert!(
        fetcher(allowed)
            .get(&url)
            .await
            .unwrap()
            .status()
            .is_success()
    );
}

/// A CA ("corporate root") and an HTTPS server for `corp.test` signed by it.
struct Corp {
    ca_pem: String,
    port: u16,
}

async fn corp_https() -> Corp {
    use rcgen::{BasicConstraints, CertificateParams, CertifiedIssuer, IsCa, KeyPair};
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca = CertifiedIssuer::self_signed(ca_params, KeyPair::generate().unwrap()).unwrap();
    let leaf_key = KeyPair::generate().unwrap();
    let leaf = CertificateParams::new(vec!["corp.test".into()])
        .unwrap()
        .signed_by(&leaf_key, &ca)
        .unwrap();

    let key = rustls::pki_types::PrivateKeyDer::try_from(leaf_key.serialize_der()).unwrap();
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(vec![leaf.der().clone()], key)
    .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                if let Ok(tls) = acceptor.accept(tcp).await {
                    answer(tls, port, routes, Seen::default()).await;
                }
            });
        }
    });
    Corp {
        ca_pem: ca.pem(),
        port,
    }
}

async fn fetch_corp(
    port: u16,
    extra_ca_file: Option<&Path>,
) -> Result<reqwest::Response, pmt_core::Error> {
    let outbound = OutboundPolicy {
        extra_ca_file: extra_ca_file.map(Path::to_owned),
        ..OutboundPolicy::default()
    };
    Fetcher::with_lookup(&outbound, public_loopback(), names())?
        .get(&format!("https://corp.test:{port}/"))
        .await
}

#[tokio::test]
async fn tls_is_verified_and_extra_ca_file_roots_are_trusted() {
    let corp = corp_https().await;
    let err = fetch_corp(corp.port, None).await.unwrap_err();
    assert_eq!(
        err.code().code,
        "net.request",
        "untrusted root must fail: {err}"
    );

    let dir = tempfile::tempdir().unwrap();
    let ca = dir.path().join("corp-ca.pem");
    std::fs::write(&ca, &corp.ca_pem).unwrap();
    let response = fetch_corp(corp.port, Some(&ca)).await.unwrap();
    assert_eq!(response.text().await.unwrap(), "hello");
}

/// Set by [`corporate_root_in_the_os_store_is_trusted`] for the child run.
const CHILD_PORT: &str = "PMT_TEST_CORP_PORT";

/// The Linux system bundle is where installed roots live; OpenSSL-style
/// `SSL_CERT_FILE` points the platform loader at a bundle that holds the
/// corporate root, like `update-ca-certificates` would. The fetch runs in a
/// child process so the variable is set before the verifier loads roots.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn corporate_root_in_the_os_store_is_trusted() {
    let corp = corp_https().await;
    let dir = tempfile::tempdir().unwrap();
    let bundle = dir.path().join("ca-certificates.crt");
    std::fs::write(&bundle, &corp.ca_pem).unwrap();
    let exe = std::env::current_exe().unwrap();
    let port = corp.port;
    let output = tokio::task::spawn_blocking(move || {
        std::process::Command::new(exe)
            .args(["--exact", "os_store_child", "--ignored", "--nocapture"])
            .env("SSL_CERT_FILE", &bundle)
            .env_remove("SSL_CERT_DIR")
            .env(CHILD_PORT, port.to_string())
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    let log = String::from_utf8_lossy(&output.stdout).into_owned()
        + &String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{log}");
    assert!(
        log.contains("1 passed"),
        "the child test must have run: {log}"
    );
}

/// The child half: no `extra_ca_file`, only the OS store.
#[tokio::test]
#[ignore = "run by corporate_root_in_the_os_store_is_trusted with SSL_CERT_FILE set"]
async fn os_store_child() {
    let port: u16 = std::env::var(CHILD_PORT)
        .expect("run by the parent test")
        .parse()
        .unwrap();
    let response = fetch_corp(port, None).await.unwrap();
    assert_eq!(response.text().await.unwrap(), "hello");
}

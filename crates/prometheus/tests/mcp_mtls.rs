//! MCP client certificates in the TLS handshake (spec: plugin-system, MCP
//! mTLS and identity header): an HTTPS front that requires a client
//! certificate signed by its CA relays to the fixture's HTTP server;
//! `mcp test` gets through it only with `client_cert` (a combined PEM, or
//! with `client_key`, an encrypted PKCS#8 key decrypted through
//! `client_key_password_secret`).

mod common;

use std::io::{BufRead as _, BufReader};
use std::path::Path;
use std::process::{Child, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, ExtendedKeyUsagePurpose, IsCa, KeyPair,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio::net::{TcpListener, TcpStream};

use self::common::{append_config, mock_home, ok, run, stderr};

const KEY_PASSWORD: &str = "key-pass-123";

/// The fixture serving streamable HTTP on loopback (killed on drop).
struct Fixture {
    child: Child,
    addr: String,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn fixture() -> Fixture {
    let mut child = std::process::Command::new(pmt_mcp::testkit::fixture())
        .args(["--http", "127.0.0.1:0"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let addr = line.trim().strip_prefix("listening ").unwrap().to_owned();
    Fixture { child, addr }
}

/// A CA, a server certificate for 127.0.0.1 and a client certificate,
/// all written under `dir`.
struct Pki {
    ca: CertificateDer<'static>,
    server_cert: CertificateDer<'static>,
    server_key: Vec<u8>,
}

fn pki(dir: &Path) -> Pki {
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca = CertifiedIssuer::self_signed(ca_params, KeyPair::generate().unwrap()).unwrap();

    let server_key = KeyPair::generate().unwrap();
    let mut server = CertificateParams::new(vec!["127.0.0.1".to_owned()]).unwrap();
    server.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let server_cert = server.signed_by(&server_key, &ca).unwrap();

    let client_key = KeyPair::generate().unwrap();
    let mut client = CertificateParams::new(Vec::<String>::new()).unwrap();
    client.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    let client_cert = client.signed_by(&client_key, &ca).unwrap();

    std::fs::write(dir.join("ca.pem"), ca.pem()).unwrap();
    std::fs::write(dir.join("client.pem"), client_cert.pem()).unwrap();
    std::fs::write(
        dir.join("combined.pem"),
        format!("{}{}", client_cert.pem(), client_key.serialize_pem()),
    )
    .unwrap();
    std::fs::write(
        dir.join("client-key.enc.pem"),
        encrypted(&client_key.serialize_der()),
    )
    .unwrap();
    Pki {
        ca: ca.der().clone(),
        server_cert: server_cert.der().clone(),
        server_key: server_key.serialize_der(),
    }
}

/// `der` (a PKCS#8 key) encrypted with [`KEY_PASSWORD`] (PBES2: PBKDF2
/// SHA-256, AES-256-CBC), as `ENCRYPTED PRIVATE KEY` PEM.
fn encrypted(der: &[u8]) -> String {
    use pkcs8::der::EncodePem as _;
    let info = pkcs8::PrivateKeyInfo::try_from(der).unwrap();
    let params =
        pkcs8::pkcs5::pbes2::Parameters::pbkdf2_sha256_aes256cbc(2048, b"saltsalt", &[7; 16])
            .unwrap();
    let encrypted = info
        .encrypt_with_params(params, KEY_PASSWORD.as_bytes())
        .unwrap();
    let info = pkcs8::EncryptedPrivateKeyInfo::try_from(encrypted.as_bytes()).unwrap();
    info.to_pem(pkcs8::LineEnding::LF).unwrap()
}

/// An HTTPS front on loopback requiring a client certificate signed by
/// the CA, relaying each connection to `upstream`.
struct Front {
    _runtime: tokio::runtime::Runtime,
    port: u16,
    /// Handshakes refused (no or an untrusted client certificate).
    refused: Arc<AtomicUsize>,
}

fn front(pki: &Pki, upstream: String) -> Front {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = rustls::RootCertStore::empty();
    roots.add(pki.ca.clone()).unwrap();
    let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
        Arc::new(roots),
        provider.clone(),
    )
    .build()
    .unwrap();
    let key = PrivateKeyDer::try_from(pki.server_key.clone()).unwrap();
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_client_cert_verifier(verifier)
        .with_single_cert(vec![pki.server_cert.clone()], key)
        .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let listener = runtime.block_on(TcpListener::bind("127.0.0.1:0")).unwrap();
    let port = listener.local_addr().unwrap().port();
    let refused = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&refused);
    runtime.spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let (acceptor, upstream) = (acceptor.clone(), upstream.clone());
            let counter = Arc::clone(&counter);
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(tcp).await else {
                    counter.fetch_add(1, Ordering::SeqCst);
                    return;
                };
                if let Ok(mut plain) = TcpStream::connect(&upstream).await {
                    let _ = tokio::io::copy_bidirectional(&mut tls, &mut plain).await;
                }
            });
        }
    });
    Front {
        _runtime: runtime,
        port,
        refused,
    }
}

/// A home whose server `tls` is the front, trusting the CA, with `extra`
/// server keys.
fn home_with(dir: &Path, port: u16, extra: &str) -> tempfile::TempDir {
    let home = mock_home();
    let path = |name: &str| dir.join(name).display().to_string().replace('\\', "/");
    append_config(
        home.path(),
        &format!(
            "\n[network]\nextra_ca_file = \"{}\"\n\n[mcp.servers.tls]\ntransport = \"http\"\nurl = \"https://127.0.0.1:{port}/mcp\"\ntrust = \"full\"\n{}",
            path("ca.pem"),
            extra.replace("{dir}", &path("")),
        ),
    );
    home
}

#[test]
fn the_client_certificate_is_presented_in_the_handshake() {
    let dir = tempfile::tempdir().unwrap();
    let pki = pki(dir.path());
    let fixture = fixture();
    let front = front(&pki, fixture.addr.clone());

    // Without a certificate the front refuses the handshake.
    let bare = home_with(dir.path(), front.port, "");
    let out = run(bare.path(), &["mcp", "test", "tls"]);
    assert_eq!(out.status.code(), Some(69), "{}", stderr(&out));
    assert!(
        front.refused.load(Ordering::SeqCst) > 0,
        "refused in the handshake"
    );

    // A combined PEM (certificate and key in one file).
    let combined = home_with(
        dir.path(),
        front.port,
        "client_cert = \"{dir}combined.pem\"\n",
    );
    let out = run(combined.path(), &["mcp", "test", "tls"]);
    assert!(out.status.success(), "{}", stderr(&out));

    // The certificate with an encrypted key, decrypted through the
    // password secret.
    let encrypted = home_with(
        dir.path(),
        front.port,
        "client_cert = \"{dir}client.pem\"\nclient_key = \"{dir}client-key.enc.pem\"\nclient_key_password_secret = \"TLS_KEY_PASSWORD\"\n",
    );
    ok(&common::run_with_stdin(
        encrypted.path(),
        &["secrets", "set", "TLS_KEY_PASSWORD"],
        &format!("{KEY_PASSWORD}\n"),
    ));
    let out = run(encrypted.path(), &["mcp", "test", "tls"]);
    assert!(out.status.success(), "{}", stderr(&out));
}

#[test]
fn an_encrypted_key_without_its_password_fails_naming_the_key() {
    let dir = tempfile::tempdir().unwrap();
    pki(dir.path());
    let home = home_with(
        dir.path(),
        9,
        "client_cert = \"{dir}client.pem\"\nclient_key = \"{dir}client-key.enc.pem\"\n",
    );
    let out = run(home.path(), &["mcp", "test", "tls"]);
    assert_eq!(out.status.code(), Some(69), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("the key is encrypted and no password was given"),
        "{}",
        stderr(&out)
    );
}

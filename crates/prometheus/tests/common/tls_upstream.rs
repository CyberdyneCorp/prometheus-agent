//! An HTTPS upstream for credential-injection tests (spec:
//! security-boundaries, Credential injection at the egress proxy).

use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// An HTTPS server for `localhost` signed by a test CA (written to
/// `ca_file`) that records each request head and answers `200 hello`.
pub fn upstream(ca_file: &Path) -> (u16, Arc<Mutex<Vec<String>>>) {
    let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca = rcgen::CertifiedIssuer::self_signed(ca_params, rcgen::KeyPair::generate().unwrap())
        .unwrap();
    std::fs::write(ca_file, ca.pem()).unwrap();
    let key = rcgen::KeyPair::generate().unwrap();
    let leaf = rcgen::CertificateParams::new(vec!["localhost".to_owned()])
        .unwrap()
        .signed_by(&key, &ca)
        .unwrap();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = Arc::new(
        rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![leaf.der().clone()],
                rustls::pki_types::PrivateKeyDer::try_from(key.serialize_der()).unwrap(),
            )
            .unwrap(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let heads = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&heads);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut tcp = stream;
            let mut conn = rustls::ServerConnection::new(Arc::clone(&config)).unwrap();
            let mut tls = rustls::Stream::new(&mut conn, &mut tcp);
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 4096];
            while !buffer.windows(4).any(|w| w == b"\r\n\r\n") {
                match tls.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buffer.extend_from_slice(&chunk[..n]),
                }
            }
            seen.lock()
                .unwrap()
                .push(String::from_utf8_lossy(&buffer).into_owned());
            let _ = tls.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
            );
            conn.send_close_notify();
            let _ = conn.complete_io(&mut tcp);
        }
    });
    (port, heads)
}

/// Asserts one injected exchange: the child saw a placeholder (`seen`),
/// its request to the mapped host succeeded (`answer`) and reached it
/// with the real `token`, a request elsewhere (`elsewhere`) was refused,
/// and the upstream got exactly one request.
pub fn assert_injected(
    (seen, answer, elsewhere): (&str, &str, &str),
    heads: &Mutex<Vec<String>>,
    token: &str,
) {
    assert!(seen.starts_with("pmt-egress-"), "the child saw {seen:?}");
    assert!(!seen.contains(token));
    assert_eq!(answer, "hello", "the child's request got {answer:?}");
    // The placeholder bound for a host it is not mapped to never leaves.
    assert!(
        elsewhere.contains("credential misdirected"),
        "{elsewhere:?}"
    );
    // (The refused tunnel's connection was opened before its request was
    // read; it carried nothing.)
    let heads: Vec<String> = heads
        .lock()
        .unwrap()
        .iter()
        .filter(|h| !h.is_empty())
        .cloned()
        .collect();
    assert_eq!(heads.len(), 1, "{heads:?}");
    assert!(
        heads[0].contains(&format!("Bearer {token}")),
        "{}",
        heads[0]
    );
    assert!(!heads[0].contains(seen));
}

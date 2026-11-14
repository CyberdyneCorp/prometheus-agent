//! The gateway's remote client attachment (spec: messaging-gateway, Remote
//! client attachment): the listener and device revocation belong to the
//! gateway, so CI also runs this file on a build without the dashboard.

#![cfg(feature = "gateway")]

mod common;

use std::path::Path;
use std::sync::Arc;

use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::http::HeaderValue;

use common::ws::{close_code, request_on};
use common::{daemon, mock_home, ok, run};

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn hello() -> Value {
    json!({"jsonrpc": "2.0", "id": 0, "method": "rpc.hello", "params": {
        "client": "desktop", "client_version": "test", "protocol": {"major": 1, "minor": 0},
        "capabilities": {"server_requests": true, "events": []}}})
}

/// A TLS WebSocket to the remote listener at `port`, trusting its
/// self-signed certificate, with `token`.
async fn remote(
    home: &Path,
    port: u16,
    query: &str,
    token: &str,
) -> tokio_tungstenite::WebSocketStream<tokio_rustls::client::TlsStream<tokio::net::TcpStream>> {
    use rustls::pki_types::pem::PemObject as _;
    let cert =
        rustls::pki_types::CertificateDer::from_pem_file(home.join("run/remote-cert.pem")).unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert).unwrap();
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let tls = tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(name, tcp)
        .await
        .unwrap();
    let mut request = format!("wss://localhost:{port}/{query}")
        .into_client_request()
        .unwrap();
    let headers = request.headers_mut();
    headers.insert(
        "Authorization",
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    );
    headers.insert(
        "Sec-WebSocket-Protocol",
        HeaderValue::from_static("prometheus-rpc.v1"),
    );
    tokio_tungstenite::client_async(request, tls)
        .await
        .unwrap()
        .0
}

/// Scenario: Disabled by default (messaging-gateway, Remote client
/// attachment).
#[test]
fn no_remote_listener_is_bound_by_default() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let status = ok(&run(home.path(), &["--json", "daemon", "status"]));
    assert!(!status.contains("listener:remote"), "{status}");
}

/// Scenario: Revoked device (messaging-gateway, Remote client attachment):
/// a bound device speaks the client protocol, a profile outside its
/// binding closes 4403, and revocation closes 4401 now and on reconnect.
#[test]
fn the_remote_listener_admits_paired_devices_only() {
    let home = mock_home();
    std::fs::write(
        home.path().join("daemon.toml"),
        "[daemon.remote]\nenabled = true\nlisten = \"127.0.0.1:0\"\n",
    )
    .unwrap();
    let paired = ok(&run(
        home.path(),
        &[
            "--json",
            "daemon",
            "pair-device",
            "laptop",
            "--profile-binding",
            "default",
        ],
    ));
    let token = serde_json::from_str::<Value>(&paired).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_owned();
    let _daemon = daemon::start(home.path(), &[]);
    let port = daemon::bound_port(home.path(), "listener:remote");
    let rt = runtime();
    rt.block_on(async {
        let mut outside = remote(home.path(), port, "?profiles=work", &token).await;
        assert_eq!(close_code(&mut outside).await, Some(4403));
        let mut bad = remote(home.path(), port, "", "pmtd_nope").await;
        assert_eq!(close_code(&mut bad).await, Some(4401));
        let mut ws = remote(home.path(), port, "", &token).await;
        let answer = request_on(&mut ws, &hello()).await;
        assert!(answer.get("result").is_some(), "{answer}");
        ok(&run(home.path(), &["daemon", "revoke-device", "laptop"]));
        assert_eq!(close_code(&mut ws).await, Some(4401));
        let mut again = remote(home.path(), port, "", &token).await;
        assert_eq!(close_code(&mut again).await, Some(4401));
    });
}

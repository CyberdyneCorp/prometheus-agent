//! Remote clients of the daemon (spec: web-dashboard, Paired device
//! credentials for remote clients; Remote desktop clients attach through
//! the dashboard listener; Desktop workspace in the browser; Webapp
//! sign-out and insecure-context notice; Webapp file staging and
//! terminals; messaging-gateway, Remote client attachment).

#![cfg(feature = "dashboard")]

mod common;

use std::path::Path;
use std::sync::Arc;

use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::http::HeaderValue;

use common::api::Request;
use common::dashboard::{self, Signed, close_code, get, path_of, request_on, sign_in};
use common::{daemon, mock_home, ok, run, stderr, stdout};

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

/// Pairs device `name` through a pairing code exchanged on the dashboard
/// listener; its credential.
fn pair(home: &Path, port: u16, name: &str) -> String {
    let printed = ok(&run(home, &["--json", "dashboard", "pair", "--name", name]));
    let code = serde_json::from_str::<Value>(&printed).unwrap()["code"]
        .as_str()
        .unwrap()
        .to_owned();
    let exchanged = Request::new("POST", "/api/devices/pair")
        .json(&json!({"code": code}))
        .send(port);
    assert_eq!(exchanged.status, 200, "{exchanged:?}");
    let again = Request::new("POST", "/api/devices/pair")
        .json(&json!({"code": code}))
        .send(port);
    assert_eq!(again.status, 401, "a pairing code works once");
    exchanged.json()["token"].as_str().unwrap().to_owned()
}

/// Scenario: Revoking a lost laptop.
#[test]
fn a_revoked_device_is_closed_with_4401_and_refused_afterwards() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let opened = dashboard::open(home.path(), &[]);
    let token = pair(home.path(), opened.port, "old-laptop");
    let bearer = format!("Bearer {token}");
    let listed = ok(&run(home.path(), &["dashboard", "devices"]));
    assert!(listed.contains("old-laptop  driver"), "{listed}");
    let path = "/api/profiles/default/sessions";
    let read = Request::new("GET", path)
        .header("Authorization", &bearer)
        .send(opened.port);
    assert_eq!(read.status, 200, "{read:?}");
    let rt = runtime();
    rt.block_on(async {
        let mut ws = dashboard::socket(
            opened.port,
            "/api/rpc",
            &[("Authorization", &bearer)],
            "prometheus-rpc.v1",
        )
        .await;
        let answer = request_on(&mut ws, &hello()).await;
        assert!(answer.get("result").is_some(), "{answer}");
        let revoked = ok(&run(
            home.path(),
            &["dashboard", "devices", "revoke", "old-laptop"],
        ));
        assert!(revoked.contains("Revoked device 'old-laptop'"), "{revoked}");
        let started = std::time::Instant::now();
        assert_eq!(close_code(&mut ws).await, Some(4401));
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    });
    let after = Request::new("GET", path)
        .header("Authorization", &bearer)
        .send(opened.port);
    assert_eq!(after.status, 401);
    let missing = run(
        home.path(),
        &["dashboard", "devices", "revoke", "old-laptop"],
    );
    assert_ne!(missing.status.code(), Some(0));
}

/// Scenario: Expired pairing code (the 10-minute expiry is proven with a
/// paused clock in pmt-gateway; here an unknown code issues nothing).
#[test]
fn a_bad_pairing_code_gets_401_and_issues_no_credential() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let opened = dashboard::open(home.path(), &[]);
    let refused = Request::new("POST", "/api/devices/pair")
        .json(&json!({"code": "ABCDEF0123"}))
        .send(opened.port);
    assert_eq!(refused.status, 401, "{refused:?}");
    assert!(!refused.body.contains("token"), "{refused:?}");
    assert_eq!(
        ok(&run(home.path(), &["dashboard", "devices"])).trim(),
        "no paired devices"
    );
}

/// Scenario: Webapp disabled.
#[test]
fn the_webapp_is_off_by_default() {
    let home = mock_home();
    let out = run(home.path(), &["webapp", "--status"]);
    assert_eq!(out.status.code(), Some(69), "{out:?}");
    assert!(
        stderr(&out).contains("webapp disabled (set webapp.enabled = true)"),
        "{out:?}"
    );
    let _daemon = daemon::start(home.path(), &[]);
    let opened = dashboard::open(home.path(), &[]);
    let signed = sign_in(&opened);
    assert_eq!(get(opened.port, "/app", &signed.cookie).status, 404);
}

fn webapp_home() -> tempfile::TempDir {
    let home = mock_home();
    std::fs::write(
        home.path().join("daemon.toml"),
        "[dashboard]\nport = 0\n[webapp]\nenabled = true\n",
    )
    .unwrap();
    home
}

/// Scenario: Opening from the CLI (the link lands on `/app` and works
/// once); the status names the address.
#[test]
fn the_webapp_link_lands_on_app_once() {
    let home = webapp_home();
    let _daemon = daemon::start(home.path(), &[]);
    let opened = ok(&run(home.path(), &["--json", "webapp", "--no-open"]));
    let doc: Value = serde_json::from_str(&opened).unwrap();
    let login = doc["login_url"].as_str().unwrap();
    let url = doc["url"].as_str().unwrap();
    assert!(url.ends_with("/app"), "{doc}");
    assert!(
        doc["warning"].is_null(),
        "loopback is a secure context: {doc}"
    );
    let port: u16 = url
        .rsplit(':')
        .next()
        .unwrap()
        .split('/')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let first = Request::new("GET", &path_of(login)).send(port);
    assert_eq!(first.status, 303, "{first:?}");
    let location = first
        .headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("location"))
        .unwrap();
    assert_eq!(location.1, "/app");
    let cookie = dashboard::set_cookie(&first)
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let app = get(port, "/app", &cookie);
    assert_eq!(app.status, 200);
    assert!(app.body.contains("<div id=\"app\""), "{}", app.body);
    assert_eq!(
        Request::new("GET", &path_of(login)).send(port).status,
        401,
        "single use"
    );
    let status = ok(&run(home.path(), &["webapp", "--status"]));
    assert_eq!(status.trim(), format!("webapp at {url}"));
}

/// Scenario: Sign out every tab (browser sessions end and their sockets
/// close with 4401; paired devices stay signed in).
#[test]
fn signing_out_ends_every_browser_session_but_not_devices() {
    let home = webapp_home();
    let _daemon = daemon::start(home.path(), &[]);
    let opened = dashboard::open(home.path(), &[]);
    let tabs: Vec<Signed> = (0..3)
        .map(|_| sign_in(&dashboard::open(home.path(), &[])))
        .collect();
    let device = pair(home.path(), opened.port, "desk");
    let rt = runtime();
    rt.block_on(async {
        let mut sockets = Vec::new();
        for tab in &tabs {
            let origin = format!("http://127.0.0.1:{}", opened.port);
            let protocols = format!("prometheus-events, csrf.{}", tab.csrf);
            sockets.push(
                dashboard::socket(
                    opened.port,
                    "/api/events",
                    &[("Cookie", &tab.cookie), ("Origin", &origin)],
                    &protocols,
                )
                .await,
            );
        }
        let out = run(home.path(), &["webapp", "--sign-out"]);
        assert!(
            stdout(&out).contains("Signed out 3 browser session(s)"),
            "{out:?}"
        );
        for socket in &mut sockets {
            assert_eq!(close_code(socket).await, Some(4401));
        }
    });
    for tab in &tabs {
        assert_eq!(get(opened.port, "/api/session", &tab.cookie).status, 401);
    }
    let still = Request::new("GET", "/api/profiles")
        .header("Authorization", &format!("Bearer {device}"))
        .send(opened.port);
    assert_eq!(still.status, 200, "paired devices stay signed in");
}

/// Scenario: Oversized drop (refused by its declared length before any
/// byte is read or written).
#[test]
fn a_staged_file_over_16_mib_is_refused_with_413() {
    use std::io::{Read as _, Write as _};
    let home = webapp_home();
    let _daemon = daemon::start(home.path(), &[]);
    let opened = dashboard::open(home.path(), &[]);
    let signed = sign_in(&opened);
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", opened.port)).unwrap();
    write!(
        stream,
        "POST /api/profiles/default/stage HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nCookie: {}\r\n\
         X-Prometheus-CSRF: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        opened.port,
        signed.cookie,
        signed.csrf,
        20 * 1024 * 1024
    )
    .unwrap();
    let mut answer = String::new();
    let _ = stream.read_to_string(&mut answer);
    assert!(answer.starts_with("HTTP/1.1 413"), "{answer}");
    let blobs = home.path().join("profiles/default/blobs");
    let stored = std::fs::read_dir(&blobs).map_or(0, Iterator::count);
    assert_eq!(stored, 0, "nothing reached the blob store");
    let small = dashboard::call(
        opened.port,
        "POST",
        "/api/profiles/default/stage",
        &signed,
        Some(&json!({"x": 1})),
    );
    assert_eq!(small.status, 201, "{small:?}");
}

/// Scenario: Headless VPS (`prometheus serve` with TLS and portal sign-in
/// serves native clients and the dashboard on one listener until stopped).
#[test]
fn serve_runs_the_daemon_with_the_dashboard_listener_open() {
    let home = mock_home();
    let cert = dashboard::certificate(home.path());
    let issuer = common::oidc::start("acct_42");
    std::fs::write(
        home.path().join("daemon.toml"),
        format!(
            "{}[dashboard.tls]\ncert = {:?}\nkey = {:?}\n",
            common::oidc::portal_toml(&issuer, common::oidc::OWNED_CLIENT),
            home.path().join("cert.pem").to_string_lossy(),
            home.path().join("key.pem").to_string_lossy()
        ),
    )
    .unwrap();
    let token = ok(&run(
        home.path(),
        &["--json", "daemon", "pair-device", "vps-client"],
    ));
    let token = serde_json::from_str::<Value>(&token).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut serve = common::command(home.path())
        .args(["serve", "--host", "0.0.0.0", "--port", "0"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let port = daemon::bound_port(home.path(), "listener:dashboard");
    let rt = runtime();
    let health =
        format!("GET /api/health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    let (code, text) = rt.block_on(dashboard::https(port, &cert, &health));
    assert_eq!(code, 200, "{text}");
    assert!(
        text.contains(r#""auth_providers":["login_link","portal"]"#),
        "{text}"
    );
    let bearer = format!(
        "GET /api/profiles HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
    );
    let (code, text) = rt.block_on(dashboard::https(port, &cert, &bearer));
    assert_eq!(
        code, 200,
        "a native client is served on the same listener: {text}"
    );
    ok(&run(home.path(), &["daemon", "stop"]));
    let status = serve.wait().unwrap();
    assert!(status.success(), "{status:?}");
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

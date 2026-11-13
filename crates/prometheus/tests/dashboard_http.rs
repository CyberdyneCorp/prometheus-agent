//! The dashboard listener over HTTP (spec: web-dashboard): the command and
//! its defaults, the embedded SPA, the listener's lifecycle within the
//! daemon, and the security of the loopback listener (login links, the
//! session cookie, CSRF, fail-closed binds, Host and Origin checks, public
//! endpoints, password sign-in over TLS).

#![cfg(feature = "dashboard")]

mod common;

use std::sync::Arc;

use serde_json::{Value, json};

use common::api::Request;
use common::dashboard::{self, Opened, call, close_code, get, path_of, set_cookie, sign_in};
use common::{daemon, mock_home, ok, run, stderr, stdout};

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// Scenario: Default launch. Scenario: Port conflict.
#[test]
fn the_default_port_is_9120_and_a_held_port_exits_78_naming_it() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let held = std::net::TcpListener::bind("127.0.0.1:9120")
        .expect("port 9120 must be free on this machine for the default-port test");
    let refused = run(home.path(), &["dashboard", "--no-open"]);
    assert_eq!(refused.status.code(), Some(78), "{}", stderr(&refused));
    assert!(
        stderr(&refused)
            .contains("dashboard port 9120 is in use; set dashboard.port or pass --port"),
        "{}",
        stderr(&refused)
    );
    let status = run(home.path(), &["dashboard", "--status"]);
    assert_eq!(
        status.status.code(),
        Some(69),
        "nothing listens after a refused bind"
    );
    drop(held);
    let opened = dashboard::parse(&ok(&run(
        home.path(),
        &["--json", "dashboard", "--no-open"],
    )));
    assert_eq!(opened.url, "http://127.0.0.1:9120/");
    assert!(
        opened
            .login_url
            .starts_with("http://127.0.0.1:9120/login?ticket="),
        "{opened:?}"
    );
    let _ = run(home.path(), &["dashboard", "--stop"]);
}

/// Scenario: Second dashboard command; `--status` and `--stop` (Listener
/// lifecycle within the daemon).
#[test]
fn a_second_command_reuses_the_listener_and_stop_leaves_the_daemon_running() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let first = dashboard::open(home.path(), &[]);
    assert!(first.started);
    let second = dashboard::parse(&ok(&run(
        home.path(),
        &["--json", "dashboard", "--no-open"],
    )));
    assert!(!second.started, "no second listener");
    assert_eq!(second.url, first.url);
    assert_ne!(second.login_url, first.login_url, "a new login link");
    sign_in(&second);
    let status = ok(&run(home.path(), &["dashboard", "--status"]));
    assert_eq!(status, format!("{}\n", first.url));
    ok(&run(home.path(), &["dashboard", "--stop"]));
    let status = run(home.path(), &["dashboard", "--status"]);
    assert_eq!(status.status.code(), Some(69), "{}", stderr(&status));
    assert!(std::net::TcpStream::connect(("127.0.0.1", first.port)).is_err());
    let daemon = ok(&run(home.path(), &["daemon", "status"]));
    assert!(daemon.contains("running"), "{daemon}");
}

/// Listener lifecycle within the daemon: retirement closes the dashboard's
/// sockets with 1001 within 3 s.
#[test]
fn daemon_retirement_closes_dashboard_sockets_with_1001() {
    let home = mock_home();
    let daemon = daemon::start(home.path(), &[]);
    let opened = dashboard::open(home.path(), &[]);
    let signed = sign_in(&opened);
    let rt = runtime();
    let mut events = rt.block_on(dashboard::socket(
        opened.port,
        "/api/events",
        &[("Cookie", &signed.cookie)],
        &format!("prometheus-events, csrf.{}", signed.csrf),
    ));
    // The clock starts before `daemon stop` is spawned and stops after it
    // returned, so it over-counts the retirement itself: passing under 3 s
    // proves the spec's bound.
    let started = std::time::Instant::now();
    drop(daemon);
    let code = rt.block_on(close_code(&mut events));
    let elapsed = started.elapsed();
    assert_eq!(code, Some(1001));
    assert!(
        elapsed < std::time::Duration::from_secs(3),
        "the dashboard closed after {elapsed:?}"
    );
    let gone = std::net::TcpStream::connect(("127.0.0.1", opened.port));
    assert!(gone.is_err(), "the listener is shut down");
}

fn header<'a>(answer: &'a common::api::Answer, name: &str) -> Option<&'a str> {
    answer
        .headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// Scenario: Unknown asset. Scenario: Deep link reload. The SPA is served
/// from the binary with no frontend files on disk.
#[test]
fn the_embedded_spa_is_served_with_hashed_assets() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let opened = dashboard::open(home.path(), &[]);
    let shell = Request::new("GET", "/sessions/42").send(opened.port);
    assert_eq!(shell.status, 200);
    assert_eq!(header(&shell, "cache-control"), Some("no-store"));
    let script = shell
        .body
        .split('"')
        .find(|part| part.starts_with("/assets/app-") && part.to_ascii_lowercase().ends_with(".js"))
        .unwrap()
        .to_owned();
    let asset = Request::new("GET", &script).send(opened.port);
    assert_eq!(asset.status, 200);
    assert_eq!(
        header(&asset, "cache-control"),
        Some("public, max-age=31536000, immutable")
    );
    assert!(
        header(&asset, "content-type")
            .unwrap()
            .starts_with("text/javascript")
    );
    let missing = Request::new("GET", "/assets/missing-abc123.js").send(opened.port);
    assert_eq!(missing.status, 404);
    assert!(missing.body.is_empty(), "no HTML body: {}", missing.body);
}

/// Scenario: Behind a proxy at /prometheus.
#[test]
fn a_base_path_prefixes_every_asset_url() {
    let home = mock_home();
    std::fs::write(
        home.path().join("daemon.toml"),
        "[dashboard]\nbase_path = \"/prometheus\"\n",
    )
    .unwrap();
    let _daemon = daemon::start(home.path(), &[]);
    let opened = dashboard::open(home.path(), &[]);
    assert!(opened.url.ends_with("/prometheus/"), "{opened:?}");
    let page = Request::new("GET", "/prometheus/").send(opened.port);
    assert_eq!(page.status, 200);
    let urls: Vec<&str> = page
        .body
        .split('"')
        .filter(|part| part.contains("/assets/"))
        .collect();
    assert_eq!(urls.len(), 2, "{}", page.body);
    for url in urls {
        assert!(url.starts_with("/prometheus/assets/"), "{url}");
        assert_eq!(Request::new("GET", url).send(opened.port).status, 200);
    }
    assert_eq!(
        Request::new("GET", "/sessions").send(opened.port).status,
        404
    );
    let signed = sign_in(&opened);
    assert_eq!(
        get(opened.port, "/prometheus/api/session", &signed.cookie).status,
        200
    );
}

/// Scenario: Reused login link. The cookie is `HttpOnly`, `SameSite=Strict`,
/// `Path=/` and expires after 12 idle hours.
#[test]
fn a_login_link_signs_in_once_with_a_strict_http_only_cookie() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let opened = dashboard::open(home.path(), &[]);
    let path = path_of(&opened.login_url);
    let first = Request::new("GET", &path).send(opened.port);
    assert_eq!(first.status, 303);
    assert_eq!(header(&first, "location"), Some("/"));
    let cookie = set_cookie(&first).unwrap();
    for attribute in ["HttpOnly", "SameSite=Strict", "Path=/", "Max-Age=43200"] {
        assert!(cookie.contains(attribute), "{cookie}");
    }
    let again = Request::new("GET", &path).send(opened.port);
    assert_eq!(again.status, 401);
    assert!(set_cookie(&again).is_none());
    assert_eq!(again.json()["error"], "unauthorized");
}

/// Scenario: Page source has no token; unauthenticated API calls get 401
/// with the login hint.
#[test]
fn served_pages_carry_no_credential() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let opened = dashboard::open(home.path(), &[]);
    let signed = sign_in(&opened);
    let ticket = opened.login_url.rsplit('=').next().unwrap();
    let token = signed.cookie.split('=').nth(1).unwrap();
    for (path, cookie) in [
        ("/", ""),
        ("/", signed.cookie.as_str()),
        ("/chat", signed.cookie.as_str()),
    ] {
        let page = Request::new("GET", path)
            .header("Cookie", cookie)
            .send(opened.port);
        assert_eq!(page.status, 200);
        for secret in [ticket, token, signed.csrf.as_str()] {
            assert!(!page.body.contains(secret), "{path} leaks a credential");
        }
    }
    let anonymous = Request::new("GET", "/api/profiles").send(opened.port);
    assert_eq!(anonymous.status, 401);
    assert_eq!(
        anonymous.json(),
        json!({"error": "unauthorized", "login": "run 'prometheus dashboard' to get a login link"})
    );
}

/// Scenario: Cross-site form post.
#[test]
fn state_changes_need_the_sessions_csrf_token() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let opened = dashboard::open(home.path(), &[]);
    let signed = sign_in(&opened);
    let path = "/api/profiles/default/sessions/1/delete";
    let forged = Request::new("POST", path)
        .header("Cookie", &signed.cookie)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .send(opened.port);
    assert_eq!(forged.status, 403);
    assert_eq!(forged.json(), json!({"error": "csrf"}));
    let other = sign_in(&dashboard::open(home.path(), &[]));
    let wrong = Request::new("DELETE", "/api/profiles/default/sessions/ses_x")
        .header("Cookie", &signed.cookie)
        .header("X-Prometheus-CSRF", &other.csrf)
        .send(opened.port);
    assert_eq!(wrong.status, 403, "another session's token");
    let rt = runtime();
    let mut refused_ws = rt.block_on(dashboard::socket(
        opened.port,
        "/api/rpc",
        &[("Cookie", &signed.cookie)],
        "prometheus-rpc.v1",
    ));
    assert_eq!(rt.block_on(close_code(&mut refused_ws)), Some(4403));
    let allowed = call(
        opened.port,
        "DELETE",
        "/api/profiles/default/sessions/ses_x",
        &signed,
        None,
    );
    assert_eq!(allowed.status, 404, "past the CSRF check: {allowed:?}");
}

/// Scenario: LAN bind without auth.
#[test]
fn a_lan_bind_without_auth_exits_78_and_never_listens() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let out = run(
        home.path(),
        &["dashboard", "--no-open", "--host", "0.0.0.0", "--port", "0"],
    );
    assert_eq!(out.status.code(), Some(78));
    assert!(
        stderr(&out).contains(
            "Refusing to bind dashboard to 0.0.0.0: dashboard.auth.mode. There is no unauthenticated remote dashboard."
        ),
        "{}",
        stderr(&out)
    );
    assert_eq!(
        run(home.path(), &["dashboard", "--status"]).status.code(),
        Some(69)
    );
    let lan = run(
        home.path(),
        &[
            "dashboard",
            "--no-open",
            "--host",
            "192.168.1.10",
            "--port",
            "0",
        ],
    );
    assert_eq!(lan.status.code(), Some(78), "RFC 1918 is not loopback");
}

/// Scenario: DNS rebinding. A malformed Host is refused too.
#[test]
fn a_foreign_or_malformed_host_header_is_refused() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let opened = dashboard::open(home.path(), &[]);
    for host in ["evil.example:9120", "evil.example", "127.0.0.1:1", "a b"] {
        let answer = Request::new("GET", "/")
            .header("Host", host)
            .send(opened.port);
        assert_eq!(answer.status, 400, "{host}");
        assert_eq!(answer.body, "Invalid Host header");
    }
    let localhost = format!("localhost:{}", opened.port);
    assert_eq!(
        Request::new("GET", "/")
            .header("Host", &localhost)
            .send(opened.port)
            .status,
        200
    );
    let signed = sign_in(&opened);
    let host = format!("evil.example:{}", opened.port);
    let rt = runtime();
    let mut ws = rt.block_on(dashboard::socket(
        opened.port,
        "/api/rpc",
        &[("Cookie", &signed.cookie), ("Host", &host)],
        &format!("prometheus-rpc.v1, csrf.{}", signed.csrf),
    ));
    assert_eq!(rt.block_on(close_code(&mut ws)), Some(4403));
}

/// Scenario: Cross-origin WebSocket. No CORS grant; security headers on
/// every answer.
#[test]
fn only_the_dashboards_own_origin_is_served() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let opened = dashboard::open(home.path(), &[]);
    let signed = sign_in(&opened);
    let rt = runtime();
    let mut ws = rt.block_on(dashboard::socket(
        opened.port,
        "/api/rpc",
        &[
            ("Cookie", &signed.cookie),
            ("Origin", "http://localhost:3000"),
        ],
        &format!("prometheus-rpc.v1, csrf.{}", signed.csrf),
    ));
    assert_eq!(rt.block_on(close_code(&mut ws)), Some(4403));
    let mut own = rt.block_on(dashboard::rpc(opened.port, &signed));
    let ping = json!({"jsonrpc": "2.0", "id": 1, "method": "rpc.ping", "params": {}});
    assert_eq!(
        rt.block_on(dashboard::request_on(&mut own, &ping))["result"]["ok"],
        true
    );
    for path in ["/", "/api/health", "/api/profiles", "/assets/missing.js"] {
        let answer = Request::new("GET", path)
            .header("Cookie", &signed.cookie)
            .header("Origin", "http://localhost:3000")
            .send(opened.port);
        assert_eq!(
            header(&answer, "content-security-policy"),
            Some("default-src 'self'; frame-ancestors 'none'"),
            "{path}"
        );
        assert_eq!(header(&answer, "x-content-type-options"), Some("nosniff"));
        assert_eq!(header(&answer, "referrer-policy"), Some("no-referrer"));
        assert!(
            header(&answer, "access-control-allow-origin").is_none(),
            "{path}"
        );
    }
}

/// Scenario: Health probe. Exactly `/api/health` and `/login` are public.
#[test]
fn the_health_probe_is_public_and_reads_no_profile_data() {
    let home = mock_home();
    ok(&run(
        home.path(),
        &["profile", "create", "secretproject", "--no-alias"],
    ));
    let _daemon = daemon::start(home.path(), &[]);
    let opened = dashboard::open(home.path(), &[]);
    let health = Request::new("GET", "/api/health").send(opened.port);
    assert_eq!(health.status, 200);
    let body = health.json();
    assert_eq!(body["ok"], true);
    assert_eq!(body["auth_required"], true);
    let version = stdout(&run(home.path(), &["--json", "version"]));
    let version: Value = serde_json::from_str(&version).unwrap();
    assert_eq!(body["build_id"], version["build_id"], "{body} vs {version}");
    for field in ["version", "protocol_version"] {
        assert!(body[field].is_string(), "{body}");
    }
    assert!(!health.body.contains("default") && !health.body.contains("secretproject"));
    for path in [
        "/api/status",
        "/api/profiles",
        "/api/logs",
        "/api/system/platform",
    ] {
        assert_eq!(
            Request::new("GET", path).send(opened.port).status,
            401,
            "{path}"
        );
    }
}

// ---- password sign-in over TLS ------------------------------------------

/// A self-signed certificate for 127.0.0.1 written as PEM files.
fn certificate(dir: &std::path::Path) -> rustls::pki_types::CertificateDer<'static> {
    let key = rcgen::KeyPair::generate().unwrap();
    let params = rcgen::CertificateParams::new(vec!["127.0.0.1".to_owned()]).unwrap();
    let cert = params.self_signed(&key).unwrap();
    std::fs::write(dir.join("cert.pem"), cert.pem()).unwrap();
    std::fs::write(dir.join("key.pem"), key.serialize_pem()).unwrap();
    cert.der().clone()
}

/// One HTTPS request; the status line's code and the raw answer.
async fn https(
    port: u16,
    root: &rustls::pki_types::CertificateDer<'static>,
    request: &str,
) -> (u16, String) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let mut roots = rustls::RootCertStore::empty();
    roots.add(root.clone()).unwrap();
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let name = rustls::pki_types::ServerName::IpAddress(std::net::Ipv4Addr::LOCALHOST.into());
    let mut tls = connector.connect(name, tcp).await.unwrap();
    tls.write_all(request.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    let _ = tls.read_to_end(&mut raw).await;
    let text = String::from_utf8_lossy(&raw).into_owned();
    let code = text
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    (code, text)
}

fn login_post(port: u16, password: &str) -> String {
    let body = format!("password={password}");
    format!(
        "POST /login HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// An argon2id PHC hash of `password` (cheap parameters).
fn phc(password: &str) -> String {
    use argon2::PasswordHasher as _;
    let params = argon2::Params::new(1024, 1, 1, None).unwrap();
    argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params)
        .hash_password_with_salt(password.as_bytes(), b"saltsaltsaltsalt")
        .unwrap()
        .to_string()
}

/// Scenario: Password mode login.
#[test]
fn five_wrong_passwords_in_a_minute_lock_the_address_out() {
    let home = mock_home();
    let cert = certificate(home.path());
    ok(&common::run_with_stdin(
        home.path(),
        &["secrets", "set", "DASHBOARD_PASSWORD_HASH"],
        &phc("hunter22"),
    ));
    std::fs::write(
        home.path().join("daemon.toml"),
        format!(
            "[dashboard]\n[dashboard.auth]\nmode = \"password\"\n[dashboard.tls]\ncert = {:?}\nkey = {:?}\n",
            home.path().join("cert.pem"),
            home.path().join("key.pem")
        ),
    )
    .unwrap();
    let _daemon = daemon::start(home.path(), &[]);
    let opened: Opened = dashboard::open(home.path(), &[]);
    assert!(opened.url.starts_with("https://"), "{opened:?}");
    let rt = runtime();
    let (code, text) = rt.block_on(https(
        opened.port,
        &cert,
        &login_post(opened.port, "hunter22"),
    ));
    assert_eq!(code, 303, "{text}");
    assert!(
        text.contains("HttpOnly; SameSite=Strict; Path=/;") && text.contains("; Secure"),
        "{text}"
    );
    for attempt in 1..=5 {
        let (code, text) =
            rt.block_on(https(opened.port, &cert, &login_post(opened.port, "wrong")));
        assert_eq!(code, 401, "attempt {attempt}: {text}");
    }
    let (code, text) = rt.block_on(https(opened.port, &cert, &login_post(opened.port, "wrong")));
    assert_eq!(code, 429, "{text}");
    let (code, _) = rt.block_on(https(
        opened.port,
        &cert,
        &login_post(opened.port, "hunter22"),
    ));
    assert_eq!(code, 429, "locked out even with the right password");
}

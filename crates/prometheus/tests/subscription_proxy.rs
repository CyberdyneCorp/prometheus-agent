//! `prometheus gateway proxy` (spec: api-server, Subscription proxy)
//! against a mock Nous endpoint: the allowed paths go upstream with the
//! profile's own sign-in instead of the client's bearer, every other path
//! answers 404, and a LAN listener needs its client key.

#![cfg(feature = "gateway")]

mod common;

use std::fmt::Write as _;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
use pmt_core::secret::SecretString;
use pmt_providers::auth::token::TokenSet;
use pmt_providers::auth::{Credentials, Obtained};
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use common::{command, ok, run, run_with_stdin, temp_home};

fn scope(home: &Path) -> ProfileScope {
    let home = Home::open(home).unwrap();
    let dir = home.profile(&ProfileName::default_profile()).unwrap();
    ProfileScope::open(dir, &Overrides::default(), Launch::Other).unwrap()
}

/// A home whose `nous` provider is the mock at `upstream` and holds a
/// Nous sign-in with access token `nous-live-token`.
fn signed_in(upstream: &str) -> common::TestHome {
    let home = temp_home();
    ok(&run(home.path(), &["config", "path"]));
    std::fs::write(
        home.path().join("profiles/default/config.toml"),
        format!("[providers.nous]\nbase_url = \"{upstream}/v1\"\nkey_hosts = [\"127.0.0.1\"]\n"),
    )
    .unwrap();
    let tokens = TokenSet {
        access_token: SecretString::new("nous-live-token"),
        refresh_token: Some(SecretString::new("nous-refresh")),
        expires_at: Some(jiff::Timestamp::now() + jiff::SignedDuration::from_hours(24)),
        id_token: None,
    };
    let obtained = Obtained {
        source: "device".to_owned(),
        ..Obtained::default()
    };
    Credentials::new(scope(home.path()).secrets().store().clone())
        .save("nous", "default", &tokens, &obtained)
        .unwrap();
    home
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// `gateway proxy start` with `args`, once it listens on `port`.
struct Running(Child);

impl Running {
    fn start(home: &Path, port: u16, args: &[&str]) -> Self {
        let port_text = port.to_string();
        let mut full = vec!["gateway", "proxy", "start", "--port", &port_text];
        full.extend_from_slice(args);
        let child = command(home)
            .args(&full)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        while TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(Instant::now() < deadline, "the proxy never listened");
            std::thread::sleep(Duration::from_millis(50));
        }
        Self(child)
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// One HTTP/1.1 request; the status and body.
fn request(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (u16, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let own = format!("127.0.0.1:{port}");
    let host = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("host"))
        .map_or(own.as_str(), |(_, value)| *value);
    let mut text = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in headers
        .iter()
        .filter(|(n, _)| !n.eq_ignore_ascii_case("host"))
    {
        let _ = write!(text, "{name}: {value}\r\n");
    }
    text.push_str("\r\n");
    text.push_str(body);
    stream.write_all(text.as_bytes()).unwrap();
    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line).unwrap();
    let status = status_line
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let mut rest = String::new();
    reader.read_to_string(&mut rest).unwrap();
    let body = rest
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_owned())
        .unwrap_or_default();
    (status, body)
}

async fn upstream() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer nous-live-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "c1", "object": "chat.completion", "model": "Hermes-4-405B",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 11, "completion_tokens": 2, "total_tokens": 13}
        })))
        .mount(&server)
        .await;
    server
}

/// Scenarios: Disallowed path; Client bearer replaced.
#[test]
fn allowed_paths_go_upstream_with_the_profile_credential_and_others_are_404() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let server = runtime.block_on(upstream());
    let home = signed_in(&server.uri());
    let port = free_port();
    let _proxy = Running::start(home.path(), port, &["--provider", "nous"]);

    let (status, body) = request(port, "POST", "/v1/images/generations", &[], "{}");
    assert_eq!(status, 404, "{body}");
    let allowed: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        allowed["error"]["allowed_paths"],
        json!([
            "/v1/chat/completions",
            "/v1/completions",
            "/v1/embeddings",
            "/v1/models"
        ])
    );
    assert!(
        runtime
            .block_on(server.received_requests())
            .unwrap()
            .is_empty()
    );

    let ask = json!({"model": "Hermes-4-405B", "messages": [{"role": "user", "content": "hello"}]});
    let (status, body) = request(
        port,
        "POST",
        "/v1/chat/completions",
        &[("Authorization", "Bearer sk-unused")],
        &ask.to_string(),
    );
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"content\":\"hi\""), "{body}");
    let sent = runtime.block_on(server.received_requests()).unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].headers["authorization"], "Bearer nous-live-token");

    // The call is in the ledger as a subscription call.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let usage: Value = serde_json::from_str(&ok(&run(
            home.path(),
            &["--json", "usage", "--by", "provider"],
        )))
        .unwrap();
        if usage.to_string().contains("subscription:nous") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "no subscription:nous row: {usage}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Scenario: LAN requires a client key.
#[test]
fn a_lan_listener_requires_the_client_key() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let server = runtime.block_on(upstream());
    let home = signed_in(&server.uri());
    let port = free_port();
    let refused = run(
        home.path(),
        &["gateway", "proxy", "start", "--host", "0.0.0.0"],
    );
    assert_eq!(
        refused.status.code(),
        Some(77),
        "{}",
        common::stderr(&refused)
    );
    ok(&run_with_stdin(
        home.path(),
        &["secrets", "set", "PMT_PROXY_CLIENT_KEY"],
        "lan-client-key-0123456789\n",
    ));
    let _proxy = Running::start(home.path(), port, &["--host", "0.0.0.0", "--allow-lan"]);
    let ask = json!({"model": "m", "messages": []}).to_string();
    let (status, _) = request(port, "POST", "/v1/chat/completions", &[], &ask);
    assert_eq!(status, 401);
    let (status, _) = request(
        port,
        "POST",
        "/v1/chat/completions",
        &[("Authorization", "Bearer lan-client-key-0123456789")],
        &ask,
    );
    assert_eq!(status, 200);
    // Bound to every interface, a DNS name on the right port is still not
    // this proxy's own address (DNS rebinding), even with the key.
    let rebound = format!("evil.example:{port}");
    let (status, _) = request(
        port,
        "POST",
        "/v1/chat/completions",
        &[
            ("Host", rebound.as_str()),
            ("Authorization", "Bearer lan-client-key-0123456789"),
        ],
        &ask,
    );
    assert_eq!(status, 403);
}

#[test]
fn browsers_and_foreign_hosts_are_refused_and_status_reports_readiness() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let server = runtime.block_on(upstream());
    let home = signed_in(&server.uri());
    let status = ok(&run(home.path(), &["gateway", "proxy", "status"]));
    assert!(status.contains("nous: ready (expires "), "{status}");
    assert!(status.contains("xai: not logged in"), "{status}");
    let out = run(
        home.path(),
        &["gateway", "proxy", "start", "--provider", "xai"],
    );
    assert_eq!(out.status.code(), Some(78), "{}", common::stderr(&out));
    let port = free_port();
    let _proxy = Running::start(home.path(), port, &[]);
    let ask = json!({"model": "m", "messages": []}).to_string();
    let (code, _) = request(
        port,
        "POST",
        "/v1/chat/completions",
        &[("Origin", "https://evil.example")],
        &ask,
    );
    assert_eq!(code, 403);
    let (code, _) = request(
        port,
        "POST",
        "/v1/chat/completions",
        &[("Sec-Fetch-Site", "cross-site")],
        &ask,
    );
    assert_eq!(code, 403);
    assert!(
        runtime
            .block_on(server.received_requests())
            .unwrap()
            .is_empty()
    );
}

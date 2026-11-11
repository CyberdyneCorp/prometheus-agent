//! `prometheus webhooks` (spec: cli, Webhook subscriptions): routes kept
//! in the profile's state, served by the running daemon's webhook adapter
//! without a restart.

#![cfg(feature = "gateway")]

mod common;

use std::path::Path;
use std::time::Duration;

use common::gateway::{Daemon, gateway_home_with, wait};
use common::{ok, run, stderr};
use serde_json::Value;

fn json_of(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("{e}: {text}"))
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A POST without a signature; its status code.
fn unsigned_post(port: u16, path: &str) -> u16 {
    use std::io::{Read as _, Write as _};
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let request = format!(
        "POST {path} HTTP/1.1\r\nhost: localhost\r\ncontent-type: application/json\r\n\
         content-length: 2\r\nconnection: close\r\n\r\n{{}}"
    );
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    response
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0)
}

fn config(home: &Path) -> String {
    std::fs::read_to_string(home.join("profiles/default/config.toml")).unwrap()
}

/// Scenario: Subscribe prints secret once. The secret is stored and shown
/// with the route URL once; `list` shows the route without it; the route
/// is not in `config.toml`; `remove` takes the generated secret along.
#[test]
fn subscribe_prints_the_secret_once_and_list_never_shows_it() {
    let home = common::mock_home();
    let home = home.path();
    let out = run(
        home,
        &[
            "webhooks",
            "subscribe",
            "deploys",
            "--events",
            "push",
            "--deliver",
            "telegram:123",
        ],
    );
    let printed = ok(&out);
    let secret = printed
        .lines()
        .find_map(|l| l.strip_prefix("secret: "))
        .unwrap_or_else(|| panic!("no secret line: {printed}"))
        .to_owned();
    assert_eq!(secret.len(), 64, "32 random bytes as hex");
    assert!(printed.contains("/p/default/webhook/deploys"), "{printed}");
    let stored = std::fs::read_to_string(home.join("profiles/default/secrets.toml")).unwrap();
    assert!(
        stored.contains(&secret),
        "the secret is in the secrets store"
    );
    assert!(
        !config(home).contains("deploys"),
        "routes are state, not config"
    );
    let listed = ok(&run(home, &["webhooks", "list"]));
    assert!(listed.contains("deploys"), "{listed}");
    assert!(
        listed.contains("push") && listed.contains("telegram:123"),
        "{listed}"
    );
    assert!(!listed.contains(&secret), "list never shows the secret");
    let listed = json_of(&ok(&run(home, &["--json", "webhooks", "list"])));
    assert!(!listed.to_string().contains(&secret));
    assert_eq!(listed["subscriptions"][0]["secret_name"], "WEBHOOK_DEPLOYS");
    // A second subscription of the name is refused.
    let again = run(home, &["webhooks", "subscribe", "deploys"]);
    assert_eq!(again.status.code(), Some(78), "{}", stderr(&again));
    ok(&run(home, &["webhooks", "remove", "deploys"]));
    assert!(!ok(&run(home, &["webhooks", "list"])).contains("deploys"));
    let secrets = ok(&run(home, &["secrets", "list"]));
    assert!(!secrets.contains("WEBHOOK_DEPLOYS"), "{secrets}");
    let gone = run(home, &["webhooks", "remove", "deploys"]);
    assert_eq!(gone.status.code(), Some(66), "{}", stderr(&gone));
}

/// A subscription made while the daemon runs is served at once; `test`
/// posts a signed sample through the listener and prints the session the
/// delivery started.
#[test]
fn test_delivers_a_signed_sample_through_the_running_daemon() {
    let home = gateway_home_with(
        "[[rule]]\nrepeat = true\ntext = \"build looked at\"\n",
        "\n[gateway.platforms.webhook]\nenabled = true\n",
    );
    let home = home.path();
    let port = free_port();
    std::fs::write(
        home.join("daemon.toml"),
        format!("[gateway.http]\nlisten = \"127.0.0.1:{port}\"\n"),
    )
    .unwrap();
    let daemon = Daemon::start(home);
    wait("the webhook listener", Duration::from_secs(30), || {
        std::net::TcpStream::connect(("127.0.0.1", port)).is_ok()
    });
    ok(&run(
        home,
        &[
            "webhooks",
            "subscribe",
            "builds",
            "--events",
            "build",
            "--deliver",
            "log",
            "--prompt",
            "build {status}",
        ],
    ));
    let payload = home.join("payload.json");
    std::fs::write(&payload, r#"{"status": "green"}"#).unwrap();
    let out = run(
        home,
        &[
            "--json",
            "webhooks",
            "test",
            "builds",
            "--payload",
            payload.to_str().unwrap(),
        ],
    );
    let report = json_of(&ok(&out));
    let session = report["session_id"]
        .as_str()
        .unwrap_or_else(|| panic!("{report}"));
    assert!(session.starts_with("ses_"), "{report}");
    wait(
        "the payload reaches the turn",
        Duration::from_secs(30),
        || {
            common::session_messages(home, session)
                .iter()
                .any(|m| m.to_string().contains("build green"))
        },
    );
    // An unsigned post to the subscribed route is refused.
    let status = unsigned_post(port, "/p/default/webhook/builds");
    assert!(
        status == 401 || status == 403,
        "unsigned post answered {status}"
    );
    daemon.stop(home);
}

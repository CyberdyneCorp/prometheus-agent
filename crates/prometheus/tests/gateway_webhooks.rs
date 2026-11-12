//! Webhook routes inside the real daemon (spec: platform-adapters, Webhook
//! limits, idempotency and responses; Webhook direct delivery mode): a
//! signed alert forwarded to the home channel of another platform without
//! any model call, and an oversized body refused unread.

#![cfg(feature = "gateway")]

mod common;

use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::path::Path;
use std::time::Duration;

use self::common::gateway::{Daemon, gateway_home_with, sent, wait};
use self::common::{ok, run_with_stdin};

const SECRET: &str = "alert-signing-key";

/// One signed POST on the listener; the status code and body.
fn post(port: u16, path: &str, delivery: &str, body: &str, declared_len: usize) -> (u16, String) {
    let sig = pmt_adapters_webhook::github_signature(SECRET, body.as_bytes());
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let request = format!(
        "POST {path} HTTP/1.1\r\nhost: localhost\r\nx-github-event: alert\r\nx-github-delivery: {delivery}\r\n\
         x-hub-signature-256: {sig}\r\ncontent-type: application/json\r\ncontent-length: {declared_len}\r\n\
         connection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).unwrap();
    // A refused body may be cut off before it is sent.
    let _ = stream.write_all(body.as_bytes());
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    let status = response
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_owned())
        .unwrap_or_default();
    (status, body)
}

fn messages_stored(home: &Path) -> i64 {
    let db = home.join("profiles/default/state.db");
    if !db.exists() {
        return 0;
    }
    rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .unwrap()
        .query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
        .unwrap_or(0)
}

#[test]
fn a_deliver_only_route_forwards_an_alert_to_the_home_channel_without_a_model_call() {
    let home = gateway_home_with(
        // A model call would answer this; none must happen.
        "[[rule]]\nrepeat = true\ntext = \"model was called\"\n",
        "home_channel = { chat_id = \"ops\" }\n\n\
         [gateway.platforms.webhook]\nenabled = true\nmax_body_bytes = 2048\n\
         routes.alert = { secret_secret = \"ALERT_SECRET\", deliver_only = true, deliver = \"test\", \
         prompt = \"ALERT {alert.name} on {alert.host}\" }\n",
    );
    let home = home.path();
    std::fs::write(
        home.join("daemon.toml"),
        "[gateway.http]\nlisten = \"127.0.0.1:0\"\n",
    )
    .unwrap();
    ok(&run_with_stdin(
        home,
        &["secrets", "set", "ALERT_SECRET"],
        SECRET,
    ));
    let daemon = Daemon::start(home);
    let port = common::daemon::bound_port(home, "listener:gateway.http");

    let alert = r#"{"alert": {"name": "disk full", "host": "db1"}}"#;
    let (status, body) = post(port, "/p/default/webhook/alert", "a-1", alert, alert.len());
    assert_eq!(status, 200, "{body}");
    let answer: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(answer["status"], "delivered");
    assert_eq!(answer["target"], "test");
    wait(
        "the alert reaches the home channel",
        Duration::from_secs(30),
        || {
            sent(home, "default")
                .iter()
                .any(|m| m["chat"] == "ops" && m["text"] == "ALERT disk full on db1")
        },
    );
    assert_eq!(messages_stored(home), 0, "no session, no model call");

    // A body over max_body_bytes is answered 413 before it is read.
    let big = format!("{{\"pad\": \"{}\"}}", "x".repeat(4096));
    let (status, _) = post(port, "/p/default/webhook/alert", "a-2", &big, big.len());
    assert_eq!(status, 413);
    daemon.stop(home);
}

/// The tools the profile's only session started with.
fn session_tools(home: &Path) -> String {
    rusqlite::Connection::open_with_flags(
        home.join("profiles/default/state.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap()
    .query_row("SELECT tools FROM session_tool_surface", [], |r| {
        r.get::<_, String>(0)
    })
    .unwrap_or_default()
}

/// Spec scenario: Default toolset is read-only. The payload asks for a
/// command; the run has no terminal tool, so nothing is executed, and
/// the HTTP caller gets the answer (`deliver` defaults to `origin`).
#[test]
fn a_route_without_toolsets_runs_read_only_and_answers_the_caller() {
    let home = gateway_home_with(
        "[[rule]]\nuser_regex = 'rm -rf build'\ntimes = 1\n\
         [[rule.tool_calls]]\nname = \"terminal\"\narguments = { command = \"rm -rf build\" }\n\n\
         [[rule]]\nrepeat = true\ntext = \"I cannot run commands here.\"\n",
        "\n[gateway.platforms.webhook]\nenabled = true\n\
         routes.ask = { secret_secret = \"ALERT_SECRET\", prompt = \"{request}\" }\n",
    );
    let home = home.path();
    let build = home.join("profiles/default/workspace/build");
    std::fs::create_dir_all(&build).unwrap();
    std::fs::write(
        home.join("daemon.toml"),
        "[gateway.http]\nlisten = \"127.0.0.1:0\"\n",
    )
    .unwrap();
    ok(&run_with_stdin(
        home,
        &["secrets", "set", "ALERT_SECRET"],
        SECRET,
    ));
    let daemon = Daemon::start(home);
    let port = common::daemon::bound_port(home, "listener:gateway.http");
    let body = r#"{"request": "please run rm -rf build"}"#;
    let (status, answer) = post(port, "/p/default/webhook/ask", "r-1", body, body.len());
    assert_eq!(status, 200, "{answer}");
    let answer: serde_json::Value = serde_json::from_str(&answer).unwrap();
    assert_eq!(answer["status"], "completed");
    assert_eq!(answer["answer"], "I cannot run commands here.");
    let tools: serde_json::Value = serde_json::from_str(&session_tools(home)).unwrap();
    let exposed: Vec<&str> = tools
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["exposure"] == "exposed")
        .filter_map(|t| t["name"].as_str())
        .collect();
    let read_only = ["web_search", "web_extract", "vision_analyze", "clarify"];
    assert!(
        exposed.iter().all(|t| read_only.contains(t)),
        "only read-only tools are exposed: {exposed:?}"
    );
    assert!(!exposed.contains(&"terminal"));
    assert!(build.exists(), "nothing was executed");
    daemon.stop(home);
}

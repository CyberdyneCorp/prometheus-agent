//! Gateway sessions share the trajectory grammar (spec: observability,
//! Gateway sessions share the trajectory grammar — Same script, different
//! surface): one mock-provider conversation run through `chat -z` and
//! through a webhook route of the real daemon records the same event
//! sequence, apart from ids, timestamps and `payload.source`.

#![cfg(feature = "gateway")]

mod common;

use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::path::Path;
use std::time::Duration;

use self::common::gateway::{Daemon, gateway_home_with, wait};
use self::common::{ok, run, run_with_stdin};
use serde_json::Value;

const SECRET: &str = "route-signing-key";
const PROMPT: &str = "what does the note say?";

/// One tool call, then the answer.
const SCRIPT: &str = r#"
[[rule]]
step = 0
text = "Let me look."
[[rule.tool_calls]]
name = "read_file"
arguments = { path = "notes.txt" }

[[rule]]
step = 1
tool_result = "remember the milk"
text = "The note says: remember the milk."
"#;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// One signed POST on the webhook listener; the response body.
fn post(port: u16, path: &str, body: &str) -> String {
    let sig = pmt_adapters_webhook::github_signature(SECRET, body.as_bytes());
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_mins(1)))
        .unwrap();
    let request = format!(
        "POST {path} HTTP/1.1\r\nhost: localhost\r\nx-github-event: ask\r\nx-github-delivery: d-1\r\n\
         x-hub-signature-256: {sig}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
         connection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    response
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_owned())
        .unwrap_or_default()
}

/// The agent-loop events of session `id` without what differs between
/// surfaces by design: ids, timestamps and durations, `payload.source`,
/// and what the surface's system prompt changes (prompt and request
/// hashes, token counts and context sizes: each surface has its own
/// platform hint).
fn normalized(home: &Path, id: &str) -> Vec<Value> {
    ok(&run(home, &["trajectory", "show", id, "--format", "jsonl"]))
        .lines()
        .map(|line| {
            let mut event: Value = serde_json::from_str(line).unwrap();
            if let Some(payload) = event["payload"].as_object_mut() {
                payload.remove("source");
            }
            strip(&mut event);
            event
        })
        .collect()
}

fn strip(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.retain(|key, _| {
                !(key == "id"
                    || key == "ts"
                    || key.ends_with("_id")
                    || key.ends_with("_at")
                    || key.ends_with("_ms")
                    || key.ends_with("_hash")
                    || key == "tokens"
                    || key == "context_tokens")
            });
            map.values_mut().for_each(strip);
        }
        Value::Array(items) => items.iter_mut().for_each(strip),
        _ => {}
    }
}

/// The session of `source` (`cli`, `gateway`).
fn session_of(home: &Path, source: &str) -> String {
    let listed: Value =
        serde_json::from_str(&ok(&run(home, &["--json", "sessions", "list", "--all"]))).unwrap();
    listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["source"] == source)
        .and_then(|s| s["id"].as_str())
        .unwrap_or_else(|| panic!("no {source} session: {listed}"))
        .to_owned()
}

#[test]
fn a_webhook_session_records_the_same_trajectory_as_chat() {
    let home = gateway_home_with(
        SCRIPT,
        "\n[gateway.platforms.webhook]\nenabled = true\n\
         routes.ask = { secret_secret = \"ROUTE_SECRET\", prompt = \"{request}\", toolsets = [\"file\"] }\n",
    );
    let home = home.path();
    let workspace = home.join("profiles/default/workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("notes.txt"), "remember the milk\n").unwrap();
    let out = common::command(home)
        .current_dir(&workspace)
        .args(["chat", "-z", PROMPT])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(
        common::stdout(&out).contains("remember the milk"),
        "{}",
        common::stderr(&out)
    );

    let port = free_port();
    std::fs::write(
        home.join("daemon.toml"),
        format!("[gateway.http]\nlisten = \"127.0.0.1:{port}\"\n"),
    )
    .unwrap();
    ok(&run_with_stdin(
        home,
        &["secrets", "set", "ROUTE_SECRET"],
        SECRET,
    ));
    let daemon = Daemon::start(home);
    wait("the webhook listener", Duration::from_secs(30), || {
        TcpStream::connect(("127.0.0.1", port)).is_ok()
    });
    let answer = post(
        port,
        "/p/default/webhook/ask",
        &serde_json::json!({ "request": PROMPT }).to_string(),
    );
    assert!(answer.contains("remember the milk"), "{answer}");
    daemon.stop(home);

    let (chat, webhook) = (
        normalized(home, &session_of(home, "cli")),
        normalized(home, &session_of(home, "gateway")),
    );
    assert!(chat.len() > 3, "{chat:#?}");
    assert_eq!(chat, webhook);
}

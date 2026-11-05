//! Per-session network egress (spec: security-boundaries, Per-session
//! network egress policy; Egress enforcement for child processes): a
//! session's sandboxed children reach the network only through its
//! loopback egress proxy, which admits only the hosts the session's policy
//! allows; each refusal is an `egress.denied` trajectory event and an
//! audit record; `/egress` and `egress.set` change the policy without
//! rebuilding the system prompt.

#![cfg(unix)]

mod common;

use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use common::{daemon, mock_home, stderr};
use serde_json::{Value, json};

/// A local HTTP server answering `hello` and counting its connections.
fn server() -> (u16, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&hits);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            counter.fetch_add(1, Ordering::SeqCst);
            let mut buffer = [0u8; 2048];
            let _ = stream.read(&mut buffer);
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
            );
        }
    });
    (port, hits)
}

/// A one-shot chat whose model runs `command` in the terminal.
fn run_command(home: &Path, command: &str, flags: &[&str]) -> std::process::Output {
    let script = home.join("egress.toml");
    std::fs::write(
        &script,
        format!(
            "[[rule]]\nstep = 0\n[[rule.tool_calls]]\nname = \"terminal\"\narguments = {{ command = '{command}' }}\n\n[[rule]]\nstep = 1\ntext = \"done\"\n"
        ),
    )
    .unwrap();
    common::command(home)
        .current_dir(home)
        .args([
            "chat",
            "--in-process",
            "--mock-script",
            script.to_str().unwrap(),
        ])
        .args(flags)
        .args(["-z", "fetch"])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

/// The `egress.denied` payloads of `session`'s trajectory.
fn denials(home: &Path, session: &str) -> Vec<Value> {
    common::ok(&common::run(
        home,
        &["trajectory", "show", session, "--format", "jsonl"],
    ))
    .lines()
    .map(|line| serde_json::from_str::<Value>(line).unwrap())
    .filter(|event| event["type"] == "egress.denied")
    .map(|event| event["payload"].clone())
    .collect()
}

/// The terminal's output, as the model saw it.
fn tool_output(home: &Path) -> String {
    common::active_messages(home)
        .iter()
        .filter(|m| m["role"] == "tool")
        .filter_map(|m| m["content"].as_str().map(str::to_owned))
        .collect()
}

/// Scenario: Allowlist session.
#[test]
fn an_allowlist_session_refuses_other_hosts_and_records_the_denial() {
    let home = mock_home();
    let home = home.path();
    let out = run_command(
        home,
        "curl -sS https://example.com/",
        &["--egress", "allowlist", "--allow-host", "api.github.com"],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let output = tool_output(home);
    assert!(!output.contains("Example Domain"), "{output}");
    let session = common::newest_session(home);
    let denied = denials(home, &session);
    assert!(
        denied.iter().any(|d| d["host"] == "example.com"
            && d["port"] == 443
            && d["requester"] == "terminal"),
        "{denied:?}\n{output}"
    );
    let audit =
        std::fs::read_to_string(home.join("profiles/default/logs/security-audit.jsonl")).unwrap();
    assert!(
        audit.contains("\"kind\":\"egress_denied\"") && audit.contains("example.com:443"),
        "{audit}"
    );
}

/// A sandboxed child reaches only allowed hosts: through the proxy the
/// allowed one answers and another is refused, and bypassing the proxy is
/// stopped by the OS sandbox.
#[test]
fn a_sandboxed_child_reaches_only_allowed_hosts() {
    let home = mock_home();
    let home = home.path();
    common::append_config(home, "\n[security.egress]\nallow_private = true\n");
    let (allowed, allowed_hits) = server();
    let (other, other_hits) = server();
    let command = format!(
        "curl -sS http://127.0.0.1:{allowed}/; echo; curl -sS http://127.0.0.1:{other}/; echo; curl -sS --noproxy \"*\" --max-time 5 http://127.0.0.1:{other}/; echo end"
    );
    let allow = format!("127.0.0.1:{allowed}");
    let out = run_command(
        home,
        &command,
        &["--egress", "allowlist", "--allow-host", &allow],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let output = tool_output(home);
    assert!(output.contains("hello"), "{output}");
    assert!(output.contains("end"), "{output}");
    assert_eq!(allowed_hits.load(Ordering::SeqCst), 1, "{output}");
    assert_eq!(other_hits.load(Ordering::SeqCst), 0, "{output}");
    let denied = denials(home, &common::newest_session(home));
    assert!(
        denied
            .iter()
            .any(|d| d["port"] == other && d["reason"] == "not_allowlisted"),
        "{denied:?}"
    );
}

fn prompt_hash(home: &Path, session: &str) -> Option<String> {
    let db = rusqlite::Connection::open_with_flags(
        home.join("profiles/default/state.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    db.query_row(
        "SELECT system_prompt_hash FROM sessions WHERE id = ?1",
        [session],
        |row| row.get(0),
    )
    .unwrap()
}

async fn submit(client: &pmt_rpc::client::Client, session: &str, text: &str) {
    client
        .call(
            "prompt.submit",
            json!({"session_id": session, "text": text}),
        )
        .await
        .unwrap();
}

async fn finished(inbox: &mut pmt_rpc::client::Inbox) {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(30), inbox.events.recv())
            .await
            .expect("a turn finishes within 30 s")
            .expect("the connection is open");
        if event.event == "turn.finished" {
            return;
        }
    }
}

/// Scenario: Session change keeps cache.
#[test]
fn changing_egress_mid_session_keeps_the_system_prompt() {
    let home = mock_home();
    let script = home.path().join("plain.toml");
    std::fs::write(&script, "[[rule]]\nrepeat = true\ntext = \"ok\"\n").unwrap();
    let _daemon = daemon::start(home.path(), &[]);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let (client, mut inbox) = daemon::connect(home.path(), "default").await;
        let created = client
            .call(
                "session.create",
                json!({"setup": {"mock_script": script.to_str().unwrap()}}),
            )
            .await
            .unwrap();
        let session = created["session_id"].as_str().unwrap().to_owned();
        submit(&client, &session, "first").await;
        finished(&mut inbox).await;
        let before = prompt_hash(home.path(), &session).expect("the first turn set it");

        let dispatched = client
            .call(
                "command.dispatch",
                json!({"session_id": session, "name": "egress", "args": "allow pypi.org"}),
            )
            .await
            .unwrap();
        assert!(dispatched.to_string().contains("pypi.org"), "{dispatched}");
        let state = client
            .call(
                "egress.set",
                json!({"session_id": session, "mode": "allowlist", "deny": ["example.com"]}),
            )
            .await
            .unwrap();
        assert_eq!(state["mode"], "allowlist", "{state}");
        assert_eq!(state["allow"], json!(["pypi.org"]), "{state}");
        assert_eq!(state["deny"], json!(["example.com"]), "{state}");

        submit(&client, &session, "second").await;
        finished(&mut inbox).await;
        assert_eq!(
            prompt_hash(home.path(), &session).as_deref(),
            Some(before.as_str()),
            "the system prompt was not rebuilt"
        );
    });
}

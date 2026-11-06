//! Delegation in messaging chats of the real daemon, driven through the
//! debug-build file adapter (platform `test`) (spec: subagent-delegation,
//! User-launched background sessions; Gateway delegation mode and
//! one-shot joining).

#![cfg(feature = "gateway")]

mod common;

use std::path::Path;
use std::time::Duration;

use self::common::gateway::{Daemon, gateway_home_with, send, texts, wait, wait_said};

fn db(home: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open_with_flags(
        home.join("profiles/default/state.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap()
}

/// Every stored message of `session`.
fn contents(home: &Path, session: &str) -> Vec<String> {
    let conn = db(home);
    let mut stmt = conn
        .prepare("SELECT coalesce(content, '') FROM messages WHERE session_id = ?1")
        .unwrap();
    stmt.query_map([session], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

/// The chat's routed session.
fn chat_session(home: &Path) -> String {
    db(home)
        .query_row(
            "SELECT session_id FROM gateway_sessions ORDER BY rowid LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap()
}

const BACKGROUND_RULES: &str = r#"
[[rule]]
user = "summarise today's error logs"
latency_ms = 1500
text = "three errors, all timeouts"

[[rule]]
repeat = true
text = "fast done"
"#;

/// Scenario: Result delivered to the same chat.
#[test]
fn a_background_result_is_posted_to_the_chat_and_not_into_its_session() {
    let home = gateway_home_with(BACKGROUND_RULES, "");
    let home = home.path();
    let daemon = Daemon::start(home);
    send(home, "default", "u1", "hello");
    wait_said(home, "default", "fast done");
    send(home, "default", "u1", "/bg summarise today's error logs");
    wait_said(home, "default", "Background task #1 started");
    send(home, "default", "u1", "still there?");
    wait("the foreground answered", Duration::from_secs(30), || {
        texts(home, "default")
            .iter()
            .filter(|t| *t == "fast done")
            .count()
            == 2
    });
    wait_said(
        home,
        "default",
        "✅ Background task #1 complete\n\nthree errors, all timeouts",
    );
    let chat = contents(home, &chat_session(home));
    assert!(
        !chat.iter().any(|c| c.contains("three errors")),
        "the chat's session never holds the task's answer: {chat:?}"
    );
    daemon.stop(home);
}

const JOINING_RULES: &str = r#"
[[rule]]
user = "please delegate"
text = "Let me split this up."
[[rule.tool_calls]]
name = "delegate_task"
arguments = { tasks = [{ goal = "child one" }, { goal = "child two" }] }

[[rule]]
user = "child one"
latency_ms = 300
text = "one done"

[[rule]]
user = "child two"
latency_ms = 600
text = "two done"

[[rule]]
tool_result_regex = "(?s)one done.*two done"
text = "both children reported"

[[rule]]
repeat = true
text = "unexpected"
"#;

/// Scenario: One message per turn on Telegram (a platform set up like
/// Telegram's defaults: no tool progress, interim assistant messages off,
/// under the default `auto` mode).
#[test]
fn a_platform_without_interim_messages_waits_for_its_children_and_replies_once() {
    let home = gateway_home_with(
        JOINING_RULES,
        "\n[gateway.display.platforms.test]\ninterim_assistant_messages = false\n\
         tool_progress = \"off\"\n",
    );
    let home = home.path();
    let daemon = Daemon::start(home);
    send(home, "default", "u1", "please delegate");
    wait_said(home, "default", "both children reported");
    std::thread::sleep(Duration::from_secs(2));
    let said = texts(home, "default");
    assert_eq!(said, ["both children reported"], "one message for the turn");
    let session = chat_session(home);
    let stored = contents(home, &session);
    assert!(
        !stored.iter().any(|c| c.contains("delegation_completed")),
        "no later completion turn: {stored:?}"
    );
    let tools: String = db(home)
        .query_row(
            "SELECT tools FROM session_prompts WHERE session_id = ?1",
            [&session],
            |r| r.get(0),
        )
        .unwrap();
    let tools: serde_json::Value = serde_json::from_str(&tools).unwrap();
    let delegate = tools
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "delegate_task")
        .unwrap();
    assert!(
        delegate["parameters"]["properties"].get("action").is_none(),
        "no live-control actions offered: {delegate}"
    );
    daemon.stop(home);
}

//! `/background` in the line interface (spec: messaging-gateway,
//! Background sessions from chat): the task runs in a session of its own
//! while the chat keeps answering, and its result arrives as a
//! notification.

#![cfg(unix)]

mod common;

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use common::{ok, run, temp_home};
use expectrl::session::OsSession;
use expectrl::{Eof, Expect};

const SCRIPT: &str = r#"
[[rule]]
user = "slow research"
latency_ms = 2000
text = "the findings"

[[rule]]
user = "endless research"
latency_ms = 60000
text = "never seen"

[[rule]]
repeat = true
text = "ok"
"#;

fn spawn(home: &Path, script: &Path) -> OsSession {
    let mut cmd: Command = common::command(home);
    cmd.args(["--set", "ui.interface=line", "--no-color"])
        .args(["--mock-script", script.to_str().unwrap()])
        .current_dir(home)
        .env("TERM", "xterm");
    let mut session = OsSession::spawn(cmd).unwrap();
    session.set_expect_timeout(Some(Duration::from_secs(30)));
    session
}

/// Scenario: Main chat stays responsive (in the line interface).
#[test]
fn a_background_task_reports_while_the_chat_keeps_answering() {
    let home = temp_home();
    for (key, value) in [("model.provider", "mock"), ("model.default", "mock")] {
        ok(&run(home.path(), &["config", "set", key, value]));
    }
    let script = home.path().join("script.toml");
    std::fs::write(&script, SCRIPT).unwrap();
    let mut p = spawn(home.path(), &script);
    p.expect("› ").unwrap();
    p.send_line("/bg slow research").unwrap();
    p.expect("Background task #1 started: slow research (id bg_")
        .unwrap();
    p.expect("› ").unwrap();
    p.send_line("/agents").unwrap();
    p.expect("Background tasks").unwrap();
    p.expect("#1 bg_").unwrap();
    p.expect("› ").unwrap();
    p.send_line("hello").unwrap();
    let answered = p.expect("ok").unwrap();
    assert!(
        !String::from_utf8_lossy(answered.before()).contains("Background task complete"),
        "the chat answered while the task ran"
    );
    p.expect("Background task #1 complete").unwrap();
    p.expect("the findings").unwrap();
    p.send_line("/exit").unwrap();
    p.expect(Eof).unwrap();
    let listed = ok(&run(home.path(), &["--json", "sessions", "list", "--all"]));
    let listed: serde_json::Value = serde_json::from_str(&listed).unwrap();
    let sessions = listed["sessions"].as_array().unwrap();
    assert_eq!(
        sessions.len(),
        2,
        "the task had a session of its own: {listed}"
    );
    // Scenario (subagent-delegation, User-launched background sessions):
    // `source = background`, the launching session as parent for lineage
    // only (its own root), the answer never in the launching session, and
    // `chat -c` resumes the launching session, not the task's.
    let task = sessions.iter().find(|s| s["source"] == "background");
    let task = task.unwrap_or_else(|| panic!("no background session: {listed}"));
    let chat = sessions.iter().find(|s| s["source"] == "cli").unwrap();
    assert_eq!(task["parent_session_id"], chat["id"]);
    assert_eq!(task["lineage_kind"], "background");
    assert_eq!(task["root_session_id"], task["id"]);
    let id = chat["id"].as_str().unwrap();
    let export = ok(&run(home.path(), &["sessions", "export", id]));
    assert!(export.contains("hello"), "{export}");
    assert!(!export.contains("the findings"), "{export}");
    let resumed = common::command(home.path())
        .current_dir(home.path())
        .args(["--json", "--mock-script", script.to_str().unwrap()])
        .args(["-z", "again", "-c"])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let resumed: serde_json::Value = serde_json::from_str(&ok(&resumed)).unwrap();
    assert_eq!(resumed["session_id"], chat["id"], "{resumed}");
}

/// Scenario (subagent-delegation, User-launched background sessions):
/// `/agents` stops a background task; the chat says so.
#[test]
fn agents_stops_a_background_task() {
    let home = temp_home();
    for (key, value) in [("model.provider", "mock"), ("model.default", "mock")] {
        ok(&run(home.path(), &["config", "set", key, value]));
    }
    let script = home.path().join("script.toml");
    std::fs::write(&script, SCRIPT).unwrap();
    let mut p = spawn(home.path(), &script);
    p.expect("› ").unwrap();
    p.send_line("/bg endless research").unwrap();
    p.expect("Background task #1 started").unwrap();
    p.expect("› ").unwrap();
    p.send_line("/tasks stop #1").unwrap();
    p.expect("Stopping background task #1").unwrap();
    p.expect("❌ Background task #1 failed: stopped").unwrap();
    p.send_line("/exit").unwrap();
    p.expect(Eof).unwrap();
}

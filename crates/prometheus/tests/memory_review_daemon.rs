//! The memory review summary on the daemon's surfaces (spec: memory,
//! Reviewable memory changes per session): a one-shot chat attached to the
//! daemon prints it as its session ends, and a messaging conversation is
//! told when `/new` ends the session and when `/resume` brings it back.

#![cfg(feature = "gateway")]

mod common;

use common::gateway::{Daemon, gateway_home_with, send, texts, wait, wait_said};
use common::{daemon, mock_home, run, stderr};

const SUMMARY: &str = "Memory: +2 ~1 -0 (review with /memory review)";

const SCRIPT: &str = r#"
[[rule]]
step = 0
[[rule.tool_calls]]
name = "memory"
arguments = { target = "user", operations = [{ action = "add", content = "Lives in Lisbon." }, { action = "add", content = "Works in UTC." }] }

[[rule]]
step = 1
[[rule.tool_calls]]
name = "memory"
arguments = { target = "user", operations = [{ action = "patch", old_text = "Lisbon", content = "Lives in Porto." }] }

[[rule]]
step = 2
text = "noted"

[[rule]]
repeat = true
text = "fast done"
"#;

/// Scenario: Session end shows a summary (`chat -z` attached to the
/// daemon).
#[test]
fn an_attached_one_shot_chat_prints_the_summary_as_its_session_ends() {
    let home = mock_home();
    let script = home.path().join("mock.toml");
    std::fs::write(&script, SCRIPT).unwrap();
    let _daemon = daemon::start(home.path(), &[]);
    let out = run(
        home.path(),
        &[
            "chat",
            "--mock-script",
            script.to_str().unwrap(),
            "-z",
            "remember where I live",
        ],
    );
    let err = stderr(&out);
    assert!(out.status.success(), "{err}");
    assert!(err.contains(SUMMARY), "{err}");
}

/// Scenario: Session end shows a summary (a messaging conversation): told
/// at `/new` and again at `/resume` of that session.
#[test]
fn a_messaging_chat_is_told_the_summary_at_new_and_at_resume() {
    let home = gateway_home_with(SCRIPT, "");
    let home = home.path();
    let daemon = Daemon::start(home);
    send(home, "default", "u1", "remember where I live");
    wait_said(home, "default", "noted");
    let first = common::newest_session(home);
    send(home, "default", "u1", "/new");
    wait_said(home, "default", "New session started.");
    let said = texts(home, "default");
    let summary = said.iter().position(|t| t == SUMMARY);
    let started = said.iter().position(|t| t == "New session started.");
    assert!(summary.is_some() && summary < started, "{said:?}");

    send(home, "default", "u1", &format!("/resume {first}"));
    wait(
        "the summary again",
        std::time::Duration::from_mins(1),
        || {
            texts(home, "default")
                .iter()
                .filter(|t| *t == SUMMARY)
                .count()
                == 2
        },
    );
    daemon.stop(home);
}

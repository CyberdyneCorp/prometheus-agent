//! Crash resume in a messaging chat served by the real daemon (spec:
//! agent-turn-loop, Crash resume mid-tool-batch; messaging-gateway, Crash
//! recovery and clean shutdown marker): under the default
//! `agent.resume_on_crash = "ask"` the chat is asked by the gateway's
//! restart notice, and `/retry` resumes the journaled turn: the command
//! that was running gets the outcome-unknown result instead of running
//! again, and the answer reaches the chat.

#![cfg(all(feature = "gateway", unix))]

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use self::common::gateway::{Daemon, gateway_home_with, send, wait, wait_said};

const RULES: &str = r#"
[[rule]]
user_regex = '^deploy$'
step = 0
[[rule.tool_calls]]
name = "terminal"
arguments = { command = "touch crash-started && sleep 60 && touch crash-deployed" }

[[rule]]
step = 1
text = "checked the deploy"

[[rule]]
repeat = true
text = "fast done"
"#;

/// Where the session's terminal created `name`, if it did.
fn created(home: &Path, name: &str) -> Option<PathBuf> {
    common::files_under(home)
        .into_iter()
        .find(|p| p.file_name().is_some_and(|f| f == name))
}

#[test]
fn retry_after_a_restart_resumes_the_turn_from_its_journal() {
    let home = gateway_home_with(RULES, "");
    let home = home.path();
    let daemon = Daemon::start(home);
    send(home, "default", "u1", "deploy");
    wait("the terminal runs", Duration::from_mins(1), || {
        created(home, "crash-started").is_some()
    });
    daemon.kill();

    let daemon = Daemon::start(home);
    wait_said(home, "default", pmt_gateway::messaging::RESTARTED);
    send(home, "default", "u1", "/retry");
    wait_said(home, "default", "checked the deploy");
    daemon.stop(home);

    assert!(
        created(home, "crash-deployed").is_none(),
        "the interrupted command never ran again"
    );
    let session = common::newest_session(home);
    let all = serde_json::to_string(&common::session_messages(home, &session)).unwrap();
    assert!(all.contains("outcome unknown"), "{all}");
    assert_eq!(
        all.matches("\"content\":\"deploy\"").count(),
        1,
        "the message was resumed, not sent again: {all}"
    );
}

//! `/review` in the line interface (spec: subagent-delegation, Review
//! command): a reviewer child runs in the background on a snapshot of the
//! session's recent messages, and its findings return to the same session
//! as a `delegation_completed` input labelled with the focus.

#![cfg(unix)]

mod common;

use common::pty::{exit, prompt, spawn};
use common::{mock_home, newest_session, session_messages};
use expectrl::Expect as _;

const SCRIPT: &str = r#"
[[rule]]
user = "open a pull request for the login form"
text = "I opened pull request #7 adding the login form."

[[rule]]
user_regex = "^Review the recent work"
latency_ms = 500
text = "Finding: the login form posts without CSRF protection."

[[rule]]
user_regex = "Review: focus on security"
text = "Noted the review findings."

[[rule]]
repeat = true
text = "ok"
"#;

/// Scenario: Review returns to the session.
#[test]
fn a_review_runs_in_the_background_and_returns_to_the_session() {
    let home = mock_home();
    let script = home.path().join("script.toml");
    std::fs::write(&script, SCRIPT).unwrap();
    let mut p = spawn(home.path(), &["--mock-script", script.to_str().unwrap()]);
    prompt(&mut p);
    p.send_line("open a pull request for the login form")
        .unwrap();
    p.expect("I opened pull request #7").unwrap();
    prompt(&mut p);
    let session = newest_session(home.path());
    p.send_line("/review focus on security").unwrap();
    p.expect("Review started. Results will return here.")
        .unwrap();
    p.expect("Noted the review findings.").unwrap();
    prompt(&mut p);
    exit(p);
    let messages = session_messages(home.path(), &session);
    let completion: Vec<&str> = messages
        .iter()
        .filter(|m| m["role"] == "user")
        .filter_map(|m| m["content"].as_str())
        .filter(|c| c.contains("[delegation_completed "))
        .collect();
    assert_eq!(completion.len(), 1, "{messages:?}");
    assert!(completion[0].contains("Review: focus on security"));
    assert!(
        completion[0].contains("CSRF protection"),
        "{}",
        completion[0]
    );
    let conn = rusqlite::Connection::open(home.path().join("profiles/default/state.db")).unwrap();
    let prompt_text: String = conn
        .query_row(
            "SELECT p.tiers FROM session_prompts p JOIN sessions s ON s.id = p.session_id \
             WHERE s.source = 'subagent'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        prompt_text.contains("I opened pull request #7")
            && prompt_text.contains("open a pull request for the login form"),
        "the reviewer saw the recent messages: {prompt_text}"
    );
}

const MONITOR_SCRIPT: &str = r#"
[[rule]]
user = "please delegate"
[[rule.tool_calls]]
name = "delegate_task"
arguments = { tasks = [{ goal = "slow child work" }] }

[[rule]]
tool_result = "dispatched"
text = "dispatched it"

[[rule]]
user = "slow child work"
latency_ms = 60000
text = "never seen"

[[rule]]
user_regex = "delegation_completed"
text = "got the completion"

[[rule]]
repeat = true
text = "ok"
"#;

/// Scenario (Agents monitor and live transcripts): `/agents` lists a
/// running child, tails its live transcript and stops it.
#[test]
fn agents_lists_tails_and_stops_a_running_child() {
    let home = mock_home();
    let script = home.path().join("script.toml");
    std::fs::write(&script, MONITOR_SCRIPT).unwrap();
    let mut p = spawn(home.path(), &["--mock-script", script.to_str().unwrap()]);
    prompt(&mut p);
    p.send_line("please delegate").unwrap();
    p.expect("dispatched it").unwrap();
    prompt(&mut p);
    p.send_line("/agents").unwrap();
    p.expect("Subagents").unwrap();
    let found = p
        .expect(expectrl::Regex("sa-[0-9A-Z]{26} running"))
        .unwrap();
    let line = String::from_utf8_lossy(found.get(0).unwrap()).into_owned();
    let id = line.split(' ').next().unwrap().to_owned();
    prompt(&mut p);
    p.send_line(format!("/agents tail {id}")).unwrap();
    p.expect(format!("dispatched {id} (task 1): slow child work").as_str())
        .unwrap();
    prompt(&mut p);
    p.send_line(format!("/agents stop {id}")).unwrap();
    p.expect("siblings keep running").unwrap();
    p.expect("got the completion").unwrap();
    prompt(&mut p);
    exit(p);
}

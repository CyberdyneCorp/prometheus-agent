//! `delegate_task` from API calls (spec: subagent-delegation, Background
//! and synchronous dispatch; api-server, Run events stream): a stateless
//! API call waits for its children, and a run's event stream carries the
//! children's `subagent.start` / `subagent.complete` before it ends.

#![cfg(feature = "gateway")]

mod common;

use std::path::Path;

use serde_json::{Value, json};

use self::common::api::{self, Request, chat, run_until_terminal};
use self::common::{append_config, daemon, ok, run};

/// The parent delegates one task; the child answers; the parent says
/// whether it saw the child's answer or only a background handle.
const DELEGATING: &str = r#"
[[rule]]
user = "please delegate"
[[rule.tool_calls]]
name = "delegate_task"
arguments = { tasks = [{ goal = "child work" }] }

[[rule]]
user = "child work"
text = "child result 42"

[[rule]]
tool_result_regex = "child result 42"
text = "parent used the child result"

[[rule]]
tool_result_regex = "(?s).*"
repeat = true
text = "parent got no child result"
"#;

fn home() -> (tempfile::TempDir, u16) {
    let (home, port) = api::home("");
    let path = home.path().join("api-script.toml");
    std::fs::write(&path, DELEGATING).unwrap();
    append_config(
        home.path(),
        &format!(
            "\n[providers.mock]\nscript = {:?}\n",
            path.to_string_lossy()
        ),
    );
    (home, port)
}

/// The `delegate_task` result stored in `session`.
fn delegate_result(home: &Path, session: &str) -> String {
    common::session_messages(home, session)
        .iter()
        .find(|m| m["role"] == "tool")
        .and_then(|m| m["content"].as_str().map(str::to_owned))
        .expect("a delegate_task result")
}

/// The id of the one subagent session.
fn child_session(home: &Path) -> String {
    let listed: Value =
        serde_json::from_str(&ok(&run(home, &["--json", "sessions", "list", "--all"]))).unwrap();
    let children: Vec<&Value> = listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["source"] == "subagent")
        .collect();
    assert_eq!(children.len(), 1, "{listed:#}");
    children[0]["id"].as_str().unwrap().to_owned()
}

fn assert_waited(result: &str) {
    assert!(!result.contains("dispatched"), "{result}");
    assert!(result.contains("child result 42"), "{result}");
    assert!(
        result.contains("Stateless API calls wait for their children."),
        "{result}"
    );
}

/// Scenario (subagent-delegation): a Runs API call is synchronous; its
/// events stream reports the child, with its session and `dg-` dispatch.
#[test]
fn a_run_waits_for_its_children_and_streams_their_events() {
    let (home, port) = home();
    let _daemon = daemon::start(home.path(), &[]);
    let created = Request::new("POST", "/v1/runs")
        .auth("default")
        .json(&json!({"input": "please delegate"}))
        .send(port);
    assert_eq!(created.status, 202, "{}", created.body);
    let id = created.json()["run_id"].as_str().unwrap().to_owned();
    let done = run_until_terminal(port, "default", &id);
    assert_eq!(done["status"], "completed", "{done}");
    assert_eq!(done["output"], "parent used the child result", "{done}");
    let session = done["session_id"].as_str().unwrap();
    assert_waited(&delegate_result(home.path(), session));

    let events = Request::new("GET", &format!("/v1/runs/{id}/events"))
        .auth("default")
        .send(port)
        .events();
    let position = |name: &str| {
        events
            .iter()
            .position(|(_, e, _)| e.as_deref() == Some(name))
            .unwrap_or_else(|| panic!("no {name}: {events:?}"))
    };
    let (start, complete, end) = (
        position("subagent.start"),
        position("subagent.complete"),
        position("run.completed"),
    );
    assert!(start < complete && complete < end, "{events:?}");
    let child = child_session(home.path());
    for at in [start, complete] {
        let data: Value = serde_json::from_str(&events[at].2).unwrap();
        assert_eq!(data["child_session_id"], child.as_str(), "{data}");
        assert!(
            data["delegation_id"]
                .as_str()
                .is_some_and(|d| d.starts_with("dg-")),
            "{data}"
        );
        assert!(
            data["subagent_id"]
                .as_str()
                .is_some_and(|s| s.starts_with("sa-")),
            "{data}"
        );
    }
    let complete: Value = serde_json::from_str(&events[complete].2).unwrap();
    assert_eq!(complete["status"], "completed", "{complete}");
}

/// Scenario (subagent-delegation): a Chat Completions call is synchronous
/// and answers with the delegated work done.
#[test]
fn a_chat_completion_waits_for_its_children() {
    let (home, port) = home();
    let _daemon = daemon::start(home.path(), &[]);
    let answer = chat(
        port,
        "default",
        &json!({"messages": [{"role": "user", "content": "please delegate"}]}),
    );
    assert_eq!(answer.status, 200, "{}", answer.body);
    assert_eq!(
        answer.json()["choices"][0]["message"]["content"],
        "parent used the child result"
    );
    let session = answer.header("x-prometheus-session-id").unwrap().to_owned();
    assert_waited(&delegate_result(home.path(), &session));
}

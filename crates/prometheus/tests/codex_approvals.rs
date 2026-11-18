//! Codex app-server approvals in a daemon session (spec: model-providers,
//! Codex app-server runtime): a command Codex asks to run goes through the
//! session's own approval gate, so the same rules apply, a dangerous one
//! is asked as `approval.request`, and the answer given there is the one
//! Codex receives (`accept` / `decline` in the fake Codex's log).

#![cfg(feature = "gateway")]

mod common;

use std::path::Path;

use serde_json::{Value, json};

use self::common::api::{self, Request, run_until, run_until_terminal};
use self::common::{codex_fixture, daemon, ok, run};

/// An API-serving home whose default profile runs `openai-api` on the
/// fake Codex app-server.
fn codex_home() -> (tempfile::TempDir, std::path::PathBuf) {
    let home = api::home("");
    let codex = codex_fixture(&home.path().join("bin"));
    let codex_bin = codex.to_string_lossy().into_owned();
    for (key, value) in [
        ("model.provider", "openai-api"),
        ("model.default", "gpt-5"),
        ("model.openai_runtime", "codex_app_server"),
        ("model.codex_bin", codex_bin.as_str()),
        ("budgets.allow_unpriced", "true"),
    ] {
        ok(&run(home.path(), &["config", "set", key, value]));
    }
    (home, codex)
}

/// The approval answers the fake Codex received, as `(decision, command)`.
fn answers(codex: &Path) -> Vec<(String, String)> {
    std::fs::read_to_string(codex.parent().unwrap().join("codex-fixture.log"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter_map(|entry| {
            let decision = entry["approval"].as_str()?.to_owned();
            Some((decision, entry["command"].as_str()?.to_owned()))
        })
        .collect()
}

fn create(port: u16, input: &str) -> String {
    let answer = Request::new("POST", "/v1/runs")
        .auth("default")
        .json(&json!({"input": input}))
        .send(port);
    assert_eq!(answer.status, 202, "{}", answer.body);
    answer.json()["run_id"].as_str().unwrap().to_owned()
}

/// Answers the run's pending approval with `choice`; the approval's
/// `approval.request` event data.
fn answer_approval(port: u16, id: &str, choice: &str) -> Value {
    let waiting = run_until(port, "default", id, |r| {
        r["status"] == "waiting_for_approval"
    });
    let pending = waiting["pending_controls"][0].clone();
    let answered = Request::new("POST", &format!("/v1/runs/{id}/approval"))
        .auth("default")
        .json(&json!({"request_id": pending["request_id"],
            "execution_generation": pending["execution_generation"], "choice": choice}))
        .send(port);
    assert_eq!(answered.status, 200, "{}", answered.body);
    let events = Request::new("GET", &format!("/v1/runs/{id}/events"))
        .auth("default")
        .send(port)
        .events();
    let asked = events
        .iter()
        .find(|(_, e, _)| e.as_deref() == Some("approval.request"))
        .expect("an approval.request event");
    let asked: Value = serde_json::from_str(&asked.2).unwrap();
    assert_eq!(asked["request_id"], pending["request_id"], "{asked}");
    asked
}

#[test]
fn codex_command_approvals_go_through_the_session_gate() {
    let (home, codex) = codex_home();
    ok(&run(
        home.path(),
        &["config", "set", "approvals.deny", "[\"echo*\"]"],
    ));
    let _daemon = daemon::start(home.path(), &[]);
    let port = api::port(home.path());

    // A command a deny rule matches is declined without a prompt.
    let id = create(port, "run it");
    let done = run_until_terminal(port, "default", &id);
    assert_eq!(done["status"], "completed", "{done}");
    assert_eq!(done["output"], "codex saw: run it");
    assert!(
        done["pending_controls"]
            .as_array()
            .is_none_or(Vec::is_empty)
    );
    assert_eq!(answers(&codex), [("decline".into(), "echo hi".into())]);

    // A dangerous one is asked; an approval lets Codex run it.
    let id = create(port, "run the clean");
    answer_approval(port, &id, "once");
    let done = run_until_terminal(port, "default", &id);
    assert_eq!(done["status"], "completed", "{done}");
    assert_eq!(answers(&codex)[1], ("accept".into(), "rm -rf build".into()));

    // A denial declines it.
    let id = create(port, "run the clean again");
    answer_approval(port, &id, "deny");
    run_until_terminal(port, "default", &id);
    assert_eq!(
        answers(&codex)[2],
        ("decline".into(), "rm -rf build".into())
    );
}

//! Kanban `eval` and `evolve` tasks end to end (spec: kanban, Eval and
//! harness-evolution tasks): an eval task runs its suite through the eval
//! engine for the assignee profile and attaches the report; an evolve
//! task runs a harness-evolution job, finishes in review with the diff
//! attached, and only a person's approval adopts the harness.

#![cfg(all(feature = "eval", feature = "evolve"))]

mod common;
mod eval_common;

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use common::{append_config, mock_home, ok, run, stderr};
use eval_common::{FakeProvider, Task, json, suite};
use serde_json::Value;

const EVOLVE_MOCK: &str = include_str!("fixtures/evolve/mock.toml");

fn create(home: &Path, args: &[&str]) -> String {
    let mut all = vec!["kanban", "create"];
    all.extend(args);
    json(home, &all)["id"].as_str().unwrap().to_owned()
}

fn task(home: &Path, id: &str) -> Value {
    json(home, &["kanban", "show", id])
}

/// Ticks the dispatcher until `id` reaches one of `ends`.
fn settle(home: &Path, id: &str, ends: &[&str]) -> Value {
    let deadline = Instant::now() + Duration::from_mins(4);
    loop {
        let shown = task(home, id);
        let status = shown["task"]["status"].as_str().unwrap().to_owned();
        if ends.contains(&status.as_str()) || status == "blocked" {
            return shown;
        }
        assert!(Instant::now() < deadline, "task stayed {status}: {shown}");
        ok(&run(home, &["kanban", "dispatch", "--once"]));
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// The profile's model route is a live chat-completions server.
fn live_route(home: &Path, port: u16) {
    append_config(
        home,
        &format!(
            "\n[model]\nprovider = \"acme\"\ndefault = \"r1\"\n\n\
             [providers.acme]\nbase_url = \"http://127.0.0.1:{port}/v1\"\n\
             wire_format = \"chat_completions\"\nauth = \"none\"\nmax_retries = 0\n\n\
             [models.\"acme/r1\"]\nprice = {{ input = 0.0, output = 0.0 }}\n"
        ),
    );
}

/// Scenario: Eval task on replayed provider.
#[test]
fn a_replayed_eval_task_makes_no_live_request_and_attaches_the_report() {
    let server = FakeProvider::start("Done.");
    let home = common::temp_home();
    ok(&run(home.path(), &["config", "path"]));
    live_route(home.path(), server.port);
    let id = create(
        home.path(),
        &[
            "smoke eval",
            "--assignee",
            "default",
            "--kind",
            "eval",
            "--suite",
            "smoke",
            "--provider-mode",
            "replay",
        ],
    );
    let shown = settle(home.path(), &id, &["done"]);
    assert_eq!(shown["task"]["status"], "done", "{shown}");
    assert_eq!(server.calls(), 0, "no live provider request");
    let result = shown["task"]["result"].as_str().unwrap();
    assert!(
        result.starts_with("smoke: score 100.00") && result.contains("(replay provider)"),
        "{result}"
    );
    assert_eq!(shown["attachments"][0]["filename"], "eval-report.txt");
    let runs = json(home.path(), &["eval", "report", run_id(result)]);
    assert_eq!(runs["runs"][0]["suite"], "smoke", "{runs}");
}

/// The run id a result line names (`..., run <id> (...)`).
fn run_id(result: &str) -> &str {
    result
        .split(", run ")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap()
}

/// A live eval task runs a user suite on the profile's model route.
#[test]
fn a_live_eval_task_runs_its_suite_on_the_profile_route() {
    let server = FakeProvider::start("Total: 85.15");
    let home = common::temp_home();
    ok(&run(home.path(), &["config", "path"]));
    live_route(home.path(), server.port);
    suite(
        &home.path().join("profiles/default/evals/ledgers"),
        "",
        &[Task::new("sum", "sum the ledger", "", "Total: 85.15")],
    );
    let id = create(
        home.path(),
        &[
            "ledger eval",
            "--assignee",
            "default",
            "--kind",
            "eval",
            "--suite",
            "ledgers",
        ],
    );
    let shown = settle(home.path(), &id, &["done"]);
    assert_eq!(shown["task"]["status"], "done", "{shown}");
    assert!(server.calls() > 0, "the live route answered");
    let result = shown["task"]["result"].as_str().unwrap();
    assert!(
        result.starts_with("ledgers: score 100.00 over 1 graded of 1 tasks"),
        "{result}"
    );
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "maintenance.auto=false",
            "-c",
            "gc.auto=0",
        ])
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}");
}

fn active(home: &Path) -> String {
    json(home, &["harness", "log"])
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["active"] == true)
        .map(|v| v["version"]["id"].to_string())
        .unwrap()
}

/// Scenario: Evolve task requires approval.
#[test]
fn an_evolve_task_waits_in_review_until_a_person_approves_it() {
    let home = mock_home();
    ok(&run(
        home.path(),
        &["config", "set", "evolve.enabled", "true"],
    ));
    // An `auto` policy would adopt a CLI job; a kanban job still waits.
    ok(&run(
        home.path(),
        &["config", "set", "evolve.adoption.policy", "auto"],
    ));
    std::fs::write(home.path().join("profiles/default/mock.toml"), EVOLVE_MOCK).unwrap();
    append_config(home.path(), "\n[providers.mock]\nscript = \"mock.toml\"\n");
    let ws = home.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("ledger.csv"), "amount\n10.00\n").unwrap();
    git(&ws, &["init", "-q"]);
    git(&ws, &["add", "-A"]);
    git(&ws, &["commit", "-q", "-m", "init"]);
    std::fs::write(
        home.path().join("ledger-task.toml"),
        "statement = \"TASK-SUM the ledger\"\ndomain = \"accounting\"\nworkspace = \"ws\"\n",
    )
    .unwrap();
    let before = active(home.path());
    let workspace = format!("dir:{}", home.path().display());
    let id = create(
        home.path(),
        &[
            "evolve the ledger harness",
            "--assignee",
            "default",
            "--kind",
            "evolve",
            "--target-task",
            "ledger-task.toml",
            "--budget-k",
            "2",
            "--workspace",
            &workspace,
        ],
    );
    let shown = settle(home.path(), &id, &["review", "done"]);
    assert_eq!(shown["task"]["status"], "review", "{shown}");
    let names: Vec<&str> = shown["attachments"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|a| a["filename"].as_str())
        .collect();
    assert!(names.contains(&"harness.diff"), "{shown}");
    assert_eq!(active(home.path()), before, "unchanged until approve");
    let proposals = json(home.path(), &["evolve", "proposals"]);
    assert_eq!(proposals[0]["status"], "pending", "{proposals}");

    let approved = run(home.path(), &["kanban", "approve", &id]);
    assert!(approved.status.success(), "{}", stderr(&approved));
    let text = String::from_utf8_lossy(&approved.stdout).into_owned();
    assert!(text.contains("it applies from the next session"), "{text}");
    assert_ne!(active(home.path()), before, "approval adopted it");
    assert_eq!(task(home.path(), &id)["task"]["status"], "done");
    let proposals = json(home.path(), &["evolve", "proposals"]);
    assert_eq!(proposals[0]["status"], "adopted", "{proposals}");
}

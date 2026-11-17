//! `prometheus eval judge` and `eval report` end to end on the mock
//! provider (spec: replay-and-evaluation, Post-hoc judging; Re-judging and
//! judge identity; Eval reports and comparison).

#![cfg(feature = "eval")]

mod common;
mod eval_common;

use std::path::Path;

use common::{append_config, mock_home, ok, run, stdout};
use eval_common::{Task, json, read_json, suite};

const RUBRIC: &str = "type = \"llm\"\nrubric = \"1. It says fine. 2. It is one word.\"\n";

/// The judge model's mock script: `TIMEOUT-ME` deliverables time out.
fn judge_script(home: &Path, all_time_out: bool) {
    let rules = if all_time_out {
        "[[rule]]\nrepeat = true\nfault = \"timeout\"\n".to_owned()
    } else {
        "[[rule]]\nuser = \"TIMEOUT-ME\"\nrepeat = true\nfault = \"timeout\"\n\n[[rule]]\nuser = \"# Rubric\"\nrepeat = true\ntext = '{\"n_passed\": 1, \"n_criteria\": 2}'\n".to_owned()
    };
    std::fs::write(home.join("judge.toml"), rules).unwrap();
}

fn rows(home: &Path, run_id: &str, file: &str) -> Vec<serde_json::Value> {
    let judged = read_json(
        &home
            .join("profiles/default/evals/runs")
            .join(run_id)
            .join(file),
    );
    judged["rows"].as_array().unwrap().clone()
}

/// Scenario: Judge failure is not a zero; Scenario: Flaky re-judge keeps
/// the grade; a changed judge model is refused unless scored into its own
/// namespace.
#[test]
fn judge_failures_are_ungraded_and_a_changed_judge_is_refused() {
    let home = mock_home();
    judge_script(home.path(), false);
    append_config(
        home.path(),
        &format!(
            "\n[providers.mock]\nscript = {:?}\n",
            home.path().join("judge.toml")
        ),
    );
    let dir = home.path().join("suite");
    suite(
        &dir,
        "",
        &[
            Task::new("t1", "a", "fine", "").grader(RUBRIC),
            Task::new("t2", "b", "TIMEOUT-ME", "").grader(RUBRIC),
        ],
    );
    ok(&run(
        home.path(),
        &[
            "eval",
            "run",
            dir.to_str().unwrap(),
            "--provider",
            "mock",
            "--run-id",
            "j1",
        ],
    ));
    let judged = run(home.path(), &["eval", "judge", "j1"]);
    assert_eq!(judged.status.code(), Some(5), "{}", stdout(&judged));
    let first = rows(home.path(), "j1", "judged.json");
    assert_eq!(
        (&first[0]["frac"], &first[0]["graded"]),
        (&0.5.into(), &true.into()),
        "{first:?}"
    );
    assert_eq!(first[1]["graded"], false);
    assert!(
        first[1]["frac"].is_null() && first[1]["error"].is_string(),
        "{first:?}"
    );
    let report = json(home.path(), &["eval", "report", "j1"]);
    let stats = &report["runs"][0];
    assert_eq!(
        (&stats["score"], &stats["n"], &stats["n_ungraded"]),
        (&50.0.into(), &1.into(), &1.into()),
        "ungraded rows stay out of the denominator: {stats}"
    );

    // A flaky re-judge: every judge call now fails; the grade stays.
    judge_script(home.path(), true);
    assert_eq!(
        run(home.path(), &["eval", "judge", "j1"]).status.code(),
        Some(5)
    );
    assert_eq!(rows(home.path(), "j1", "judged.json")[0], first[0]);

    // Another judge model.
    judge_script(home.path(), false);
    ok(&run(
        home.path(),
        &["config", "set", "auxiliary.eval_judge.provider", "mock"],
    ));
    ok(&run(
        home.path(),
        &["config", "set", "auxiliary.eval_judge.model", "judge2"],
    ));
    let changed = run(home.path(), &["eval", "judge", "j1"]);
    assert_eq!(changed.status.code(), Some(1));
    let err = common::stderr(&changed);
    assert!(
        err.contains("judge changed for this run (model mock/mock")
            && err.contains("→ model mock/judge2"),
        "{err}"
    );
    assert!(
        err.contains("use --judge-namespace to score separately"),
        "{err}"
    );
    let separate = run(
        home.path(),
        &["eval", "judge", "j1", "--judge-namespace", "judge2"],
    );
    assert_eq!(separate.status.code(), Some(5));
    assert_eq!(
        rows(home.path(), "j1", "judged.judge2.json")[0]["frac"],
        0.5
    );
}

/// A `script` grader runs sandboxed on the deliverables; a task that did
/// not execute is ungraded without its grader.
#[cfg(unix)]
#[test]
fn script_graders_score_deliverables_and_unexecuted_tasks_stay_ungraded() {
    let home = mock_home();
    let dir = home.path().join("suite");
    let grader = "type = \"script\"\ncommand = [\"sh\", \"check.sh\"]\n";
    let failing = "[[rule]]\nrepeat = true\nfault = \"auth_error\"\n";
    suite(
        &dir,
        "",
        &[
            Task::new("t1", "a", "Total: 85.15", "").grader(grader),
            Task::new("t2", "b", "Total: 1", "").grader(grader),
            Task::new("t3", "c", "x", "").grader(grader).mock(failing),
        ],
    );
    for t in ["t1", "t2", "t3"] {
        std::fs::write(
            dir.join("tasks").join(t).join("grader/check.sh"),
            "if grep -q 'Total: 85.15' \"$PMT_DELIVERABLES/answer.md\"; then echo '{\"n_passed\": 2, \"n_criteria\": 2}'; else echo '{\"n_passed\": 1, \"n_criteria\": 2}'; fi\n",
        )
        .unwrap();
    }
    let out = run(
        home.path(),
        &[
            "eval",
            "run",
            dir.to_str().unwrap(),
            "--provider",
            "mock",
            "--run-id",
            "s1",
        ],
    );
    assert_eq!(
        out.status.code(),
        Some(1),
        "t3 failed to execute: {}",
        stdout(&out)
    );
    assert_eq!(
        run(home.path(), &["eval", "judge", "s1"]).status.code(),
        Some(5)
    );
    let rows = rows(home.path(), "s1", "judged.json");
    assert_eq!(rows[0]["frac"], 1.0, "{rows:?}");
    assert_eq!(rows[1]["frac"], 0.5, "{rows:?}");
    assert!(
        rows[2]["error"]
            .as_str()
            .unwrap()
            .starts_with("not executed"),
        "{rows:?}"
    );
}

/// Scenario: Harness comparison.
#[test]
fn a_report_of_two_runs_shows_the_score_delta_with_its_interval() {
    let home = mock_home();
    let script = |answers: &str| {
        std::fs::write(home.path().join("tasks.toml"), answers).unwrap();
    };
    append_config(
        home.path(),
        &format!(
            "\n[providers.mock]\nscript = {:?}\n",
            home.path().join("tasks.toml")
        ),
    );
    let dir = home.path().join("suite");
    let tasks: Vec<Task> = (1..=4)
        .map(|i| Task::new(&format!("q{i}"), &format!("question {i}"), "", "yes"))
        .collect();
    suite(&dir, "", &tasks);
    script(
        "[[rule]]\nuser = \"question 1\"\nrepeat = true\ntext = \"yes\"\n[[rule]]\nuser = \"question 2\"\nrepeat = true\ntext = \"yes\"\n[[rule]]\nrepeat = true\ntext = \"no\"\n",
    );
    ok(&run(
        home.path(),
        &["eval", "run", dir.to_str().unwrap(), "--run-id", "run-a"],
    ));
    let tree = home.path().join("tree");
    std::fs::create_dir_all(&tree).unwrap();
    std::fs::write(tree.join("SYSTEM.md"), "Answer yes when you can.").unwrap();
    ok(&run(
        home.path(),
        &["harness", "apply", tree.to_str().unwrap()],
    ));
    script("[[rule]]\nrepeat = true\ntext = \"yes\"\n");
    ok(&run(
        home.path(),
        &["eval", "run", dir.to_str().unwrap(), "--run-id", "run-b"],
    ));
    for id in ["run-a", "run-b"] {
        ok(&run(home.path(), &["eval", "judge", id]));
    }
    let text = ok(&run(home.path(), &["eval", "report", "run-a", "run-b"]));
    assert!(
        text.contains("score delta run-b - run-a: +50.00 (95% CI"),
        "{text}"
    );
    let report = json(home.path(), &["eval", "report", "run-a", "run-b"]);
    let cmp = &report["comparisons"][0];
    assert_eq!(cmp["delta"], 50.0, "{cmp}");
    assert_ne!(report["runs"][0]["harness"], report["runs"][1]["harness"]);
    assert_eq!(cmp["deltas"].as_array().unwrap().len(), 4);
    let ci = cmp["ci"].as_array().unwrap();
    assert!(
        ci[0].as_f64().unwrap() <= 50.0 && ci[1].as_f64().unwrap() >= 50.0,
        "{cmp}"
    );
}

//! `prometheus eval` end to end on the mock provider (spec:
//! replay-and-evaluation, Eval suite format; Eval run; Eval runs are
//! resumable; Answer-free boundary; Built-in smoke suite).

#![cfg(feature = "eval")]

mod common;
mod eval_common;

use common::{mock_home, ok, run, stdout};
use eval_common::{Task, json, read_json, suite};
use serde_json::Value;

/// Scenario: Missing prompt.
#[test]
fn a_task_without_a_prompt_fails_validation_naming_its_file() {
    let home = mock_home();
    let dir = home.path().join("suite");
    suite(&dir, "", &[Task::new("t1", "hi", "ok", "ok")]);
    std::fs::write(dir.join("tasks/t1/task.toml"), "files = []\n").unwrap();
    let out = run(home.path(), &["eval", "validate", dir.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(65));
    let err = common::stderr(&out);
    assert!(
        err.contains("tasks/t1/task.toml: missing key 'prompt'"),
        "{err}"
    );
}

/// Scenario: Regression in tool dispatch, scored by `eval`: the bundled
/// smoke suite runs on the mock provider and every task it can run here
/// scores 100%; results hold no score.
#[test]
fn the_bundled_smoke_suite_runs_and_judges_at_full_score() {
    let home = mock_home();
    let out = run(
        home.path(),
        &[
            "eval",
            "run",
            "smoke",
            "--provider",
            "mock",
            "--run-id",
            "smoke1",
        ],
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}{}",
        stdout(&out),
        common::stderr(&out)
    );
    let runs = home.path().join("profiles/default/evals/runs/smoke1");
    let info = read_json(&runs.join("run.json"));
    assert_eq!(
        (&info["suite"], &info["provider"], &info["cache"]),
        (
            &Value::from("smoke"),
            &Value::from("mock"),
            &Value::from("read_write")
        )
    );
    assert!(
        info["harness_hash"].as_str().is_some_and(|h| h.len() == 16),
        "{info}"
    );
    let result = std::fs::read_to_string(runs.join("tasks/tool_call/1/result.json")).unwrap();
    for key in [
        "\"ok\"",
        "\"delivered\"",
        "\"cost_usd\"",
        "\"wall_secs\"",
        "\"error\"",
    ] {
        assert!(result.contains(key), "{result}");
    }
    assert!(
        !result.contains("frac") && !result.contains("n_passed"),
        "no score: {result}"
    );
    assert!(runs.join("tasks/tool_call/1/trajectory.jsonl").is_file());
    assert!(
        runs.join("tasks/file_edit/1/deliverables/files/README.md")
            .is_file()
    );
    // The memory task is kept away from the profile's memory: skipped.
    let skipped = read_json(&runs.join("tasks/memory_write/1/result.json"));
    assert_eq!(skipped["skipped"], "needs memory");

    let judged = run(home.path(), &["eval", "judge", "smoke1"]);
    assert_eq!(
        judged.status.code(),
        Some(5),
        "the skipped task is ungraded: {}",
        stdout(&judged)
    );
    let rows = read_json(&runs.join("judged.json"));
    for row in rows["rows"].as_array().unwrap() {
        if row["task_id"] == "memory_write" {
            assert_eq!(row["graded"], false);
            assert!(row["frac"].is_null());
        } else {
            assert_eq!(row["frac"], 1.0, "{row}");
        }
    }
    let report = json(home.path(), &["eval", "report", "smoke1"]);
    assert_eq!(report["runs"][0]["score"], 100.0, "{report}");
}

fn priced_config(home: &std::path::Path, port: u16, input_price: f64) {
    std::fs::write(
        home.join("profiles/default/config.toml"),
        format!(
            "[model]\nprovider = \"acme\"\ndefault = \"r1\"\n\n\
             [providers.acme]\nbase_url = \"http://127.0.0.1:{port}/v1\"\n\
             wire_format = \"chat_completions\"\nauth = \"none\"\nmax_retries = 0\n\n\
             [models.\"acme/r1\"]\nprice = {{ input = {input_price:?}, output = 0.0 }}\n"
        ),
    )
    .unwrap();
}

/// Scenario: Budget stops a run. A probe run measures a task's input
/// tokens; the price is then set so that each task costs 0.20 USD (and
/// a call's worst case, its input, about as much), so with 0.50 the third
/// task's call is refused.
#[test]
fn the_budget_refuses_the_third_task_and_skips_the_rest() {
    let server = eval_common::FakeProvider::start("Done.");
    let home = common::temp_home();
    ok(&run(home.path(), &["config", "path"]));
    priced_config(home.path(), server.port, 100.0);
    let probe = home.path().join("probe");
    suite(&probe, "", &[Task::new("t1", "go", "", "Done.")]);
    ok(&run(
        home.path(),
        &[
            "eval",
            "run",
            probe.to_str().unwrap(),
            "--cache",
            "off",
            "--run-id",
            "p1",
        ],
    ));
    let runs = home.path().join("profiles/default/evals/runs");
    let probe_cost = read_json(&runs.join("p1/tasks/t1/1/result.json"))["cost_usd"]
        .as_f64()
        .unwrap();
    priced_config(home.path(), server.port, 100.0 * 0.2 / probe_cost);
    let dir = home.path().join("suite");
    let tasks: Vec<Task> = (1..=4)
        .map(|i| Task::new(&format!("t{i}"), "go", "", "Done."))
        .collect();
    suite(&dir, "", &tasks);
    let out = run(
        home.path(),
        &[
            "eval",
            "run",
            dir.to_str().unwrap(),
            "--budget-usd",
            "0.50",
            "--parallel",
            "1",
            "--cache",
            "off",
            "--run-id",
            "b1",
        ],
    );
    assert_eq!(
        out.status.code(),
        Some(3),
        "{}{}",
        stdout(&out),
        common::stderr(&out)
    );
    let runs = runs.join("b1/tasks");
    for t in ["t1", "t2"] {
        let r = read_json(&runs.join(t).join("1/result.json"));
        assert_eq!(r["ok"], true, "{r}");
        assert!((r["cost_usd"].as_f64().unwrap() - 0.2).abs() < 0.01, "{r}");
    }
    let third = read_json(&runs.join("t3/1/result.json"));
    assert_eq!(
        (&third["budget_stopped"], &third["ok"]),
        (&Value::from(true), &Value::from(false)),
        "{third}"
    );
    let fourth = read_json(&runs.join("t4/1/result.json"));
    assert_eq!(fourth["skipped"], "budget");
    // Its task call was refused; only the turn's short budget handoff
    // summary ran.
    assert!(third["cost_usd"].as_f64().unwrap() < 0.1, "{third}");
    let trajectory = std::fs::read_to_string(runs.join("t3/1/trajectory.jsonl")).unwrap();
    assert!(trajectory.contains("\"budget.exceeded\""), "{trajectory}");
    assert!(server.calls() >= 3);
}

/// Scenario: Tampered deliverable; a resumed run with another model is
/// refused.
#[test]
fn a_resumed_run_reruns_only_tasks_whose_deliverables_changed() {
    let home = mock_home();
    let dir = home.path().join("suite");
    suite(
        &dir,
        "",
        &[
            Task::new("t1", "a", "Total: 1", "Total: 1"),
            Task::new("t2", "b", "Total: 2", "Total: 2"),
        ],
    );
    let suite_arg = dir.to_str().unwrap();
    ok(&run(
        home.path(),
        &[
            "eval",
            "run",
            suite_arg,
            "--provider",
            "mock",
            "--run-id",
            "r1",
        ],
    ));
    let answer = home
        .path()
        .join("profiles/default/evals/runs/r1/tasks/t1/1/deliverables/answer.md");
    std::fs::write(&answer, "edited by hand").unwrap();
    let out = ok(&run(
        home.path(),
        &[
            "eval",
            "run",
            suite_arg,
            "--provider",
            "mock",
            "--run-id",
            "r1",
        ],
    ));
    assert!(
        out.contains("re-running t1: deliverables changed since completion"),
        "{out}"
    );
    assert!(!out.contains("re-running t2"), "{out}");
    assert!(
        out.contains("1 executed, 0 failed, 0 skipped, 1 resumed"),
        "{out}"
    );
    assert_eq!(std::fs::read_to_string(&answer).unwrap(), "Total: 1");
    let other = run(
        home.path(),
        &[
            "eval",
            "run",
            suite_arg,
            "--provider",
            "mock",
            "--model",
            "other",
            "--run-id",
            "r1",
        ],
    );
    assert_eq!(other.status.code(), Some(2));
    assert!(
        common::stderr(&other).contains("was started with model mock/mock"),
        "{}",
        common::stderr(&other)
    );
}

/// Scenario: Agent searches for the answer: grader files are outside the
/// task session's reach, and touching them contaminates the task.
#[cfg(unix)]
#[test]
fn a_task_reaching_for_grader_material_is_contaminated() {
    let home = mock_home();
    let dir = home.path().join("suite");
    let grader = dir.join("tasks/t1/grader");
    let find = format!(
        "[[rule]]\nstep = 0\n[[rule.tool_calls]]\nname = \"terminal\"\narguments = {{ command = 'find {} -name \"reference_answer*\" 2>&1; true' }}\n\n[[rule]]\ntext = \"searched\"\n",
        dir.display()
    );
    let read = format!(
        "[[rule]]\nstep = 0\n[[rule.tool_calls]]\nname = \"read_file\"\narguments = {{ path = '{}' }}\n\n[[rule]]\ntext = \"read it\"\n",
        grader.join("reference_answer.txt").display()
    );
    suite(
        &dir,
        "leak_globs = [\"reference_answer*\"]\n",
        &[
            Task::new("t1", "find it", "", "searched").mock(&find),
            Task::new("t2", "read it", "", "read it").mock(&read),
            Task::new("t3", "plain", "fine", "fine"),
        ],
    );
    std::fs::write(grader.join("reference_answer.txt"), "SECRET-ANSWER-42").unwrap();
    ok(&run(
        home.path(),
        &[
            "eval",
            "run",
            dir.to_str().unwrap(),
            "--provider",
            "mock",
            "--run-id",
            "c1",
        ],
    ));
    let runs = home.path().join("profiles/default/evals/runs/c1/tasks");
    let trajectory = std::fs::read_to_string(runs.join("t1/1/trajectory.jsonl")).unwrap();
    assert!(!trajectory.contains("SECRET-ANSWER-42"));
    assert!(
        !trajectory.contains("grader/reference_answer.txt\\n"),
        "the sandbox hides the grader: {trajectory}"
    );
    for t in ["t1", "t2"] {
        let r = read_json(&runs.join(t).join("1/result.json"));
        assert_eq!(r["contaminated"], true, "{t}: {r}");
    }
    let t2 = std::fs::read_to_string(runs.join("t2/1/trajectory.jsonl")).unwrap();
    assert!(
        !t2.contains("SECRET-ANSWER-42"),
        "read_file cannot read the grader: {t2}"
    );
    assert_eq!(
        read_json(&runs.join("t3/1/result.json"))["contaminated"],
        false
    );
    let judged = run(home.path(), &["eval", "judge", "c1"]);
    assert_eq!(judged.status.code(), Some(5));
    let rows = read_json(
        &home
            .path()
            .join("profiles/default/evals/runs/c1/judged.json"),
    );
    let rows = rows["rows"].as_array().unwrap();
    assert!(
        rows[0]["error"]
            .as_str()
            .unwrap()
            .starts_with("contaminated"),
        "{rows:?}"
    );
    assert_eq!(rows[2]["frac"], 1.0);
}

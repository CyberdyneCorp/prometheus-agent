//! Benchmark adapters and session capture end to end (spec:
//! replay-and-evaluation, Benchmark adapter protocol; Session-to-task
//! capture).

#![cfg(feature = "eval")]

mod common;
mod eval_common;

use std::path::Path;

use common::{append_config, mock_home, ok, run, stdout};
use eval_common::read_json;

/// Installs the sample adapter as `bench:example`.
fn install_adapter(home: &Path) -> std::path::PathBuf {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/eval/bench/example");
    let dir = home.join("profiles/default/evals/bench/example");
    std::fs::create_dir_all(&dir).unwrap();
    for file in ["adapter.toml", "adapter.py"] {
        std::fs::copy(fixture.join(file), dir.join(file)).unwrap();
    }
    dir
}

/// Scenario: Score not callable during execution.
#[cfg(unix)]
#[test]
fn an_adapter_is_never_asked_to_score_during_execution() {
    let home = mock_home();
    let script = home.path().join("tasks.toml");
    std::fs::write(&script, "[[rule]]\nuser = \"sum ledger e1\"\nrepeat = true\ntext = \"Total: 85.15\"\n[[rule]]\nrepeat = true\ntext = \"Total: 20.25\"\n").unwrap();
    append_config(
        home.path(),
        &format!("\n[providers.mock]\nscript = {script:?}\n"),
    );
    let adapter = install_adapter(home.path());
    let out = run(
        home.path(),
        &["eval", "run", "bench:example", "--run-id", "be1"],
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}{}",
        stdout(&out),
        common::stderr(&out)
    );
    let log = std::fs::read_to_string(adapter.join("calls.log")).unwrap();
    assert!(!log.contains("bench.score"), "{log}");
    assert!(
        log.contains("push: -32002"),
        "a pushed score is refused: {log}"
    );
    for method in [
        "bench.describe",
        "bench.preflight",
        "bench.tasks",
        "bench.task",
        "bench.prepare",
        "bench.deliverables",
    ] {
        assert!(log.contains(method), "{method}: {log}");
    }
    let runs = home.path().join("profiles/default/evals/runs/be1");
    let info = read_json(&runs.join("run.json"));
    assert_eq!(
        (&info["kind"], &info["suite"]),
        (&"bench".into(), &"bench:example".into())
    );
    assert!(
        runs.join("tasks/e1/1/deliverables/files/notes.txt")
            .is_file(),
        "bench.prepare filled the workspace"
    );
    assert!(
        runs.join("tasks/e1/1/deliverables/files/ledger.txt")
            .is_file()
    );

    let judged = run(home.path(), &["eval", "judge", "be1"]);
    assert_eq!(judged.status.code(), Some(5), "e2's null score is ungraded");
    let log = std::fs::read_to_string(adapter.join("calls.log")).unwrap();
    assert!(log.contains("bench.score"), "{log}");
    let rows = read_json(&runs.join("judged.json"));
    assert_eq!(rows["rows"][0]["frac"], 1.0, "{rows}");
    assert_eq!(rows["rows"][1]["graded"], false);

    ok(&run(
        home.path(),
        &["eval", "run", "bench:example:dev", "--run-id", "be2"],
    ));
    let split = home.path().join("profiles/default/evals/runs/be2/tasks");
    assert!(split.join("e1").is_dir() && !split.join("e2").exists());
}

/// Scenario: Bug report becomes a regression task.
#[test]
fn a_mis_summed_session_becomes_a_regression_task() {
    let home = mock_home();
    let work = home.path().join("ledger");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(work.join("ledger.txt"), "+100.00\n-14.85\n").unwrap();
    let script = home.path().join("wrong.toml");
    std::fs::write(
        &script,
        "[[rule]]\nrepeat = true\ntext = \"Total: 114.85\"\n",
    )
    .unwrap();
    let mut cmd = common::command(home.path());
    let out = cmd
        .args([
            "chat",
            "-z",
            "sum the ledger",
            "--provider",
            "mock",
            "--mock-script",
        ])
        .arg(&script)
        .current_dir(&work)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", common::stderr(&out));
    let session = common::newest_session(home.path());
    let suite = home.path().join("evals/accounting");
    let captured = ok(&run(
        home.path(),
        &[
            "eval",
            "capture",
            &session,
            "--suite",
            suite.to_str().unwrap(),
            "--expect",
            "Total: 85.15",
        ],
    ));
    assert!(captured.contains("captured session"), "{captured}");
    let task = std::fs::read_dir(suite.join("tasks"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let meta = std::fs::read_to_string(task.join("task.toml")).unwrap();
    assert!(meta.contains("prompt = \"sum the ledger\""), "{meta}");
    assert!(meta.contains("ledger.txt"), "{meta}");
    assert_eq!(
        std::fs::read_to_string(task.join("files/ledger.txt")).unwrap(),
        "+100.00\n-14.85\n"
    );
    let grader = std::fs::read_to_string(task.join("grader/grader.toml")).unwrap();
    assert!(
        grader.contains("type = \"exact\"") && grader.contains("expect = \"Total: 85.15\""),
        "{grader}"
    );
    ok(&run(
        home.path(),
        &["eval", "validate", suite.to_str().unwrap()],
    ));

    // A recording without workspace contents cannot provide the files.
    let rec = home.path().join("bare.pmtrec");
    ok(&run(
        home.path(),
        &["replay", "export", &session, "-o", rec.to_str().unwrap()],
    ));
    let refused = run(
        home.path(),
        &[
            "eval",
            "capture",
            rec.to_str().unwrap(),
            "--suite",
            suite.to_str().unwrap(),
            "--task-id",
            "bare",
            "--expect",
            "x",
        ],
    );
    assert_eq!(
        refused.status.code(),
        Some(65),
        "{}",
        common::stderr(&refused)
    );
    assert!(!suite.join("tasks/bare").exists());
}

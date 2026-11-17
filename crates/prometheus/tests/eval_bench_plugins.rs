//! Benchmark adapters shipped as plugins (spec: replay-and-evaluation,
//! Benchmark adapter protocol): a WASM plugin of the `bench` world and a
//! Python-bridge plugin that registered its adapter answer the same
//! protocol as a subprocess adapter, reach the call's directory only
//! through the host, and have a pushed score refused with -32002 while
//! `prometheus eval run` executes. The Python test needs uv on PATH (see
//! `pmt_pybridge::testkit`).

#![cfg(feature = "eval")]

mod common;
mod eval_common;

use std::path::Path;

use common::{append_config, mock_home, ok, run, stderr, stdout};
use eval_common::read_json;

/// The mock answers of the two ledger tasks.
fn script(home: &Path) {
    let script = home.join("tasks.toml");
    std::fs::write(
        &script,
        "[[rule]]\nuser = \"sum ledger e1\"\nrepeat = true\ntext = \"Total: 85.15\"\n[[rule]]\nrepeat = true\ntext = \"Total: 20.25\"\n",
    )
    .unwrap();
    append_config(
        home,
        &format!(
            "\n[providers.mock]\nscript = {}\n",
            eval_common::toml_path(&script)
        ),
    );
}

/// `bench:example` run, then judged: the adapter was never asked to score
/// while executing, its pushed score was refused with -32002, it wrote
/// the workspace and the run directory through the host, and only
/// `eval judge` scored (e2's null score is an ungraded row).
fn run_and_judge(home: &Path) {
    let out = run(home, &["eval", "run", "bench:example", "--run-id", "bp1"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}{}",
        stdout(&out),
        stderr(&out)
    );
    let runs = home.join("profiles/default/evals/runs/bp1");
    let info = read_json(&runs.join("run.json"));
    assert_eq!(
        (&info["kind"], &info["suite"]),
        (&"bench".into(), &"bench:example".into())
    );
    let e1 = runs.join("tasks/e1/1");
    assert_eq!(
        std::fs::read_to_string(e1.join("deliverables/files/notes.txt")).unwrap(),
        "prepared by the adapter\n",
        "bench.prepare wrote the workspace through the host"
    );
    assert!(e1.join("deliverables/files/ledger.txt").is_file());
    let calls = std::fs::read_to_string(e1.join("adapter-calls.txt")).unwrap();
    for method in [
        "bench.describe",
        "bench.preflight",
        "bench.tasks",
        "bench.task",
        "bench.prepare",
        "bench.deliverables",
    ] {
        assert!(calls.contains(method), "{method}: {calls}");
    }
    assert!(!calls.contains("bench.score"), "{calls}");
    assert!(calls.contains("push: -32002"), "refused push: {calls}");

    let judged = run(home, &["eval", "judge", "bp1"]);
    assert_eq!(judged.status.code(), Some(5), "{}", stderr(&judged));
    let rows = read_json(&runs.join("judged.json"));
    assert_eq!(rows["rows"][0]["frac"], 1.0, "{rows}");
    assert_eq!(rows["rows"][1]["graded"], false, "{rows}");

    ok(&run(
        home,
        &["eval", "run", "bench:example:dev", "--run-id", "bp2"],
    ));
    let split = home.join("profiles/default/evals/runs/bp2/tasks");
    assert!(split.join("e1").is_dir() && !split.join("e2").exists());
}

/// Scenario: Score not callable during execution (a WASM plugin
/// adapter).
#[test]
fn a_wasm_plugin_adapter_is_never_asked_to_score_during_execution() {
    let home = mock_home();
    script(home.path());
    let dir = home.path().join("profiles/default/plugins/example");
    pmt_plugins::testkit::install("sample-bench", &dir);
    std::fs::write(
        dir.join("plugin.toml"),
        "manifest_version = 1\nid = \"example\"\nversion = \"1.0.0\"\nkind = \"wasm\"\n\
         description = \"bench sample\"\nlicense = \"MIT\"\nrequires_prometheus = \"*\"\n\
         worlds = [\"bench@1.0\"]\n\n[capabilities]\n",
    )
    .unwrap();
    ok(&run(
        home.path(),
        &["plugins", "adopt", dir.to_str().unwrap(), "--grant", "all"],
    ));
    ok(&run(
        home.path(),
        &["plugins", "enable", "example", "--grant", "all"],
    ));
    run_and_judge(home.path());
}

/// Scenario: Score not callable during execution (a Python-bridge plugin
/// adapter).
#[test]
fn a_python_plugin_adapter_is_never_asked_to_score_during_execution() {
    let home = mock_home();
    pmt_pybridge::testkit::seed(home.path());
    script(home.path());
    let dir = pmt_pybridge::testkit::place(home.path(), "bench", "ledger-bench");
    ok(&run(
        home.path(),
        &["plugins", "adopt", dir.to_str().unwrap(), "--grant", "all"],
    ));
    ok(&run(
        home.path(),
        &["plugins", "enable", "ledger-bench", "--grant", "all"],
    ));
    run_and_judge(home.path());
}

/// No subprocess adapter and no plugin providing the name: exit 66.
#[test]
fn an_unknown_adapter_is_not_found() {
    let home = mock_home();
    let out = run(home.path(), &["eval", "run", "bench:nowhere"]);
    assert_eq!(out.status.code(), Some(66), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("no enabled plugin provides it"),
        "{}",
        stderr(&out)
    );
}

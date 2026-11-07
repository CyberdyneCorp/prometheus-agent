//! `prometheus verify` and `prometheus files` (spec: execution-environments,
//! Project run-recipe verification; Snapshot store management CLI).

mod common;

use common::{mock_home, ok, run};

fn project(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    for (name, text) in files {
        std::fs::write(dir.path().join(name), text).unwrap();
    }
    dir
}

/// Scenario: Detect only.
#[test]
fn detect_only_prints_the_cargo_recipe_and_runs_nothing() {
    let home = mock_home();
    let dir = project(&[("Cargo.toml", "[package]\nname = \"x\"\n")]);
    let out = ok(&run(
        home.path(),
        &["verify", "--detect-only", dir.path().to_str().unwrap()],
    ));
    let recipe: serde_json::Value = serde_json::from_str(&out).unwrap();
    let commands: Vec<&str> = recipe["phases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["command"].as_str().unwrap())
        .collect();
    assert_eq!(commands, ["cargo build", "cargo test"]);
    assert!(!dir.path().join("target").exists(), "nothing ran");
}

/// Scenario: Failing tests.
#[cfg(unix)]
#[test]
fn a_failing_test_phase_is_reported_with_its_output_tail_and_exit_1() {
    let home = mock_home();
    let dir = project(&[(
        "Makefile",
        "build:\n\t@echo built\n\ntest:\n\t@echo assertion boom; exit 1\n",
    )]);
    let out = run(home.path(), &["verify", dir.path().to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(1),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("build: passed"), "{stdout}");
    assert!(stdout.contains("test: FAILED"), "{stdout}");
    assert!(stdout.contains("| assertion boom"), "{stdout}");
    assert!(stdout.contains("verify: failed"), "{stdout}");
}

/// Clearing the snapshot store needs a confirmation: none on a pipe.
#[test]
fn clearing_the_snapshot_store_without_a_terminal_exits_4() {
    let home = mock_home();
    let out = run(home.path(), &["files", "snapshots", "clear"]);
    assert_eq!(out.status.code(), Some(4));
    ok(&run(home.path(), &["files", "snapshots", "clear", "-f"]));
    let status = ok(&run(home.path(), &["files", "snapshots", "status"]));
    assert!(
        status.starts_with("snapshot store: 0 bytes, 0 workspaces"),
        "{status}"
    );
}

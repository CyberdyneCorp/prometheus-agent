//! The offline self-improvement demo (spec: replay-and-evaluation, Offline
//! self-improvement demo; harness-evolution, Offline demo pipeline).

mod common;

use std::process::Output;

use common::{ok, stdout, temp_home};

fn demo(home: &std::path::Path, args: &[&str]) -> Output {
    common::command(home)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

fn report(dir: &std::path::Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(dir.join("demo.json")).unwrap()).unwrap()
}

/// Scenario: Resume makes no new work. The first run takes both tasks
/// through evolution offline and selects the fixed totals; the second
/// resumes without executing anything.
#[test]
fn the_evolve_demo_selects_the_fixed_totals_and_resumes_without_work() {
    let home = temp_home();
    let out = home.path().join("d");
    let first = demo(
        home.path(),
        &["demo", "--evolve", "--output", out.to_str().unwrap()],
    );
    let text = stdout(&first);
    assert_eq!(
        first.status.code(),
        Some(0),
        "{text}{}",
        common::stderr(&first)
    );
    assert!(text.contains("demo: 2 tasks completed; 0 failed"), "{text}");
    let done = report(&out);
    assert_eq!(
        (&done["tasks_completed"], &done["tasks_failed"]),
        (&2.into(), &0.into())
    );
    // k = 2, parallel: 4k rollouts and 3k proposer sessions per task.
    assert_eq!(
        (&done["executions"], &done["editor_calls"]),
        (&16.into(), &12.into()),
        "{done}"
    );
    assert_eq!(done["tasks"][0]["deliverables"][0], "Total: 85.15");
    assert_eq!(done["tasks"][1]["deliverables"][0], "Total: 20.25");
    let library = ok(&common::run(
        &out.join("home"),
        &["experience", "list", "--domain", "accounting"],
    ));
    assert!(
        library.contains("totalling a ledger"),
        "the reflection's lesson is in the library: {library}"
    );
    assert!(
        !home.path().join("profiles").exists(),
        "the user's home is untouched"
    );

    let again = demo(
        home.path(),
        &["demo", "--evolve", "--output", out.to_str().unwrap()],
    );
    assert_eq!(again.status.code(), Some(0), "{}", stdout(&again));
    let resumed = report(&out);
    assert_eq!(
        (&resumed["executions"], &resumed["editor_calls"]),
        (&0.into(), &0.into()),
        "{resumed}"
    );
    assert_eq!(resumed["tasks_completed"], 2);
}

/// Scenario: Demo resume.
#[cfg(feature = "evolve")]
#[test]
fn evolve_demo_reports_no_model_calls_when_run_again() {
    let home = temp_home();
    let out = home.path().join("d");
    let args = [
        "evolve",
        "demo",
        "--k",
        "1",
        "--mode",
        "both",
        "--output",
        out.to_str().unwrap(),
    ];
    let first = demo(home.path(), &args);
    assert_eq!(
        first.status.code(),
        Some(0),
        "{}{}",
        stdout(&first),
        common::stderr(&first)
    );
    assert!(
        stdout(&first).contains("ledger-a: ran; selected: Total: 85.15 | Total: 85.15"),
        "{}",
        stdout(&first)
    );
    let again = demo(home.path(), &args);
    let text = stdout(&again);
    assert_eq!(again.status.code(), Some(0), "{text}");
    assert!(
        text.contains("ledger-a: resumed") && text.contains("ledger-b: resumed"),
        "{text}"
    );
    assert!(text.contains("0 model calls, 0 rollouts"), "{text}");
}

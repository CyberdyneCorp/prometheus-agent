//! The bundled `smoke` eval suite (spec: replay-and-evaluation, Built-in
//! smoke suite): what the binary bundles is the suite on disk, every task
//! is graded exactly, and a broken capability scores below 100%.

mod common;

use std::path::{Path, PathBuf};
use std::process::Stdio;

use common::{command, temp_home};
use prometheus::smoke_suite;

fn suite_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("evals/smoke")
}

fn on_disk(dir: &Path, prefix: &str, out: &mut Vec<String>) {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let name = format!("{prefix}{}", entry.file_name().to_string_lossy());
        if entry.path().is_dir() {
            on_disk(&entry.path(), &format!("{name}/"), out);
        } else {
            out.push(name);
        }
    }
}

/// The binary bundles exactly the suite's files, and the suite has at
/// least 12 tasks, each with a prompt, a mock script and an `exact`
/// grader.
#[test]
fn the_bundled_suite_is_the_suite_on_disk_with_twelve_graded_tasks() {
    let mut files = Vec::new();
    on_disk(&suite_dir(), "", &mut files);
    files.sort();
    let mut bundled: Vec<&str> = smoke_suite::FILES.iter().map(|f| f.path).collect();
    bundled.sort_unstable();
    assert_eq!(bundled, files, "src/smoke_suite.rs lists every suite file");
    let suite: toml::Value = toml::from_str(smoke_suite::file("suite.toml").unwrap()).unwrap();
    assert_eq!(suite["name"].as_str(), Some("smoke"));
    let ids = smoke_suite::task_ids();
    assert!(ids.len() >= 12, "{ids:?}");
    for id in ids {
        let file = |name: &str| smoke_suite::file(&format!("tasks/{id}/{name}"));
        let task: toml::Value = toml::from_str(file("task.toml").unwrap()).unwrap();
        assert!(
            task["prompt"].as_str().is_some_and(|p| !p.is_empty()),
            "{id}"
        );
        assert!(file("mock.toml").is_some(), "{id}");
        let grader: toml::Value = toml::from_str(file("grader/grader.toml").unwrap()).unwrap();
        assert_eq!(grader["type"].as_str(), Some("exact"), "{id}");
    }
}

/// Runs the bundled `parallel_tools` task (without `missing` in its
/// workspace); its answer.
fn parallel_tools(missing: Option<&str>) -> String {
    let home = temp_home();
    let work = home.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    for name in ["README.md", "OWNERS.md"] {
        if Some(name) == missing {
            continue;
        }
        let text = smoke_suite::file(&format!("tasks/parallel_tools/files/{name}")).unwrap();
        std::fs::write(work.join(name), text).unwrap();
    }
    let mock = home.path().join("mock.toml");
    std::fs::write(
        &mock,
        smoke_suite::file("tasks/parallel_tools/mock.toml").unwrap(),
    )
    .unwrap();
    let task: toml::Value =
        toml::from_str(smoke_suite::file("tasks/parallel_tools/task.toml").unwrap()).unwrap();
    let out = command(home.path())
        .args(["chat", "-z", task["prompt"].as_str().unwrap()])
        .args(["--provider", "mock", "--mock-script"])
        .arg(&mock)
        .current_dir(&work)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// Scenario: Regression in tool dispatch: the parallel-dispatch task
/// passes its exact grade, and when one call of the batch does not bring
/// its result back (here its file is gone, standing in for a dispatch
/// regression) the task misses its grade, so the suite scores below 100%.
#[test]
fn a_broken_parallel_dispatch_misses_the_smoke_grade() {
    let expect: toml::Value =
        toml::from_str(smoke_suite::file("tasks/parallel_tools/grader/grader.toml").unwrap())
            .unwrap();
    let expect = expect["expect"].as_str().unwrap();
    assert_eq!(parallel_tools(None), expect);
    assert_ne!(parallel_tools(Some("OWNERS.md")), expect);
}

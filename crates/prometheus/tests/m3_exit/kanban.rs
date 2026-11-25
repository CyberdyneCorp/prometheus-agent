//! Task 81.3, kanban: the daemon's dispatcher runs two workers, each its
//! own profile, through dependent tasks of one board (spec: kanban, Task
//! dependencies; Worker spawn by run ticket; Single dispatcher per home;
//! Kanban toolset and grants). The child stays waiting while its parent
//! is open, then its worker starts with the parent's result in its
//! context, and both complete through `kanban_complete`.

use std::path::Path;
use std::time::Duration;

use serde_json::Value;

use crate::common::bots::wait;
use crate::common::{append_config, daemon, mock_home, ok, run};

/// The parent's worker: completes its card with the gathered data.
const GATHERER: &str = "[[rule]]\nuser_regex = 'Kanban task'\n[[rule.tool_calls]]\nname = \"kanban_complete\"\n\
    arguments = { result = \"42 rows of sales\" }\n\n[[rule]]\ntool_result_regex = 'status'\ntext = \"gathered\"\n";

/// The child's worker: completes only when its context carries the
/// parent's result.
const WRITER: &str = "[[rule]]\nuser_regex = '(?s)Kanban task.*42 rows of sales'\n[[rule.tool_calls]]\nname = \"kanban_complete\"\n\
    arguments = { result = \"report on 42 rows written\" }\n\n[[rule]]\ntool_result_regex = 'status'\ntext = \"written\"\n";

fn json(home: &Path, args: &[&str]) -> Value {
    let mut all = vec!["--json"];
    all.extend_from_slice(args);
    serde_json::from_str(&ok(&run(home, &all))).unwrap()
}

fn create(home: &Path, args: &[&str]) -> String {
    let mut all = vec!["kanban", "create"];
    all.extend_from_slice(args);
    json(home, &all)["id"].as_str().unwrap().to_owned()
}

fn show(home: &Path, id: &str) -> Value {
    json(home, &["kanban", "show", id])
}

fn worker(home: &Path, name: &str, rules: &str) {
    ok(&run(home, &["profile", "create", name, "--no-alias"]));
    std::fs::write(home.join(format!("profiles/{name}/mock.toml")), rules).unwrap();
    for (key, value) in [
        ("model.provider", "mock"),
        ("model.default", "mock"),
        ("providers.mock.script", "mock.toml"),
    ] {
        ok(&run(home, &["-p", name, "config", "set", key, value]));
    }
}

#[test]
fn the_daemon_dispatches_two_workers_through_dependent_tasks() {
    let home = mock_home();
    append_config(home.path(), "\n[kanban]\ndispatch_interval_secs = 1\n");
    worker(home.path(), "gatherer", GATHERER);
    worker(home.path(), "writer", WRITER);
    let parent = create(
        home.path(),
        &["gather the sales data", "--assignee", "gatherer"],
    );
    let child = create(
        home.path(),
        &[
            "write the sales report",
            "--assignee",
            "writer",
            "--parent",
            &parent,
        ],
    );
    assert_eq!(show(home.path(), &child)["task"]["status"], "todo");

    let _daemon = daemon::start(home.path(), &[]);
    wait("both tasks are done", Duration::from_mins(2), || {
        [&parent, &child]
            .iter()
            .all(|id| show(home.path(), id)["task"]["status"] == "done")
    });
    let (parent, child) = (show(home.path(), &parent), show(home.path(), &child));
    assert_eq!(parent["task"]["result"], "42 rows of sales");
    assert_eq!(child["task"]["result"], "report on 42 rows written");
    for (task, profile) in [(&parent, "gatherer"), (&child, "writer")] {
        let runs = task["runs"].as_array().unwrap();
        assert_eq!(runs.len(), 1, "one run each: {task}");
        assert_eq!(runs[0]["outcome"], "completed", "{task}");
        assert_eq!(runs[0]["profile"], profile, "{task}");
    }
    // The child's worker started only once its parent was done.
    let completed = parent["task"]["completed_at"].as_i64().unwrap();
    let started = child["runs"][0]["started_at"].as_i64().unwrap();
    assert!(
        started >= completed,
        "child ran at {started}, parent done at {completed}"
    );
}

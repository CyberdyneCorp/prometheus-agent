//! Automatic evolution jobs at session end, told on the session's own
//! surface (spec: harness-evolution, Automatic triggers after failed or
//! long tasks): a one-shot run's stderr, the line interface, a messaging
//! chat.

#![cfg(feature = "evolve")]

mod common;

use std::path::Path;

use common::{mock_home, ok, run, stderr};
use serde_json::Value;

/// What the user is told when a job is queued.
const QUEUED: &str = "queued after this session";

/// One tool call, then the provider refuses the credential: the session
/// ends in error after a tool call.
const FAILS_AFTER_A_TOOL: &str = r#"
[[rule]]
step = 0
[[rule.tool_calls]]
name = "memory"
arguments = { target = "user", operations = [{ action = "add", content = "Keeps the ledger in notes.txt." }] }

[[rule]]
step = 1
repeat = true
fault = "auth_error"
"#;

/// One tool call, then an answer: two iterations.
#[cfg(feature = "gateway")]
const TWO_ITERATIONS: &str = r#"
[[rule]]
step = 0
[[rule.tool_calls]]
name = "memory"
arguments = { target = "user", operations = [{ action = "add", content = "Keeps the ledger in notes.txt." }] }

[[rule]]
repeat = true
text = "noted"
"#;

/// `[evolve]` with automatic jobs on; `long` is the long-task threshold.
fn auto_config(long: u32) -> String {
    format!(
        "\n[evolve]\nenabled = true\n\n[evolve.auto]\nenabled = true\nmin_iterations = {long}\n"
    )
}

fn jobs(home: &Path) -> Vec<Value> {
    let out = ok(&run(home, &["--json", "evolve", "list"]));
    serde_json::from_str::<Value>(&out)
        .unwrap()
        .as_array()
        .unwrap()
        .clone()
}

/// Scenario: Queued job is announced (a one-shot run).
#[test]
fn a_session_that_fails_after_a_tool_call_queues_a_job_and_says_so() {
    let home = mock_home();
    common::append_config(home.path(), &auto_config(40));
    let script = home.path().join("script.toml");
    std::fs::write(&script, FAILS_AFTER_A_TOOL).unwrap();
    let out = run(
        home.path(),
        &[
            "chat",
            "--mock-script",
            script.to_str().unwrap(),
            "-z",
            "reconcile the ledger",
        ],
    );
    let err = stderr(&out);
    assert!(!out.status.success(), "{err}");
    let jobs = jobs(home.path());
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    assert_eq!(jobs[0]["trigger"], "auto");
    let id = jobs[0]["id"].as_str().unwrap();
    assert!(
        err.contains(&format!("harness evolution job {id} {QUEUED}")),
        "{err}"
    );
}

/// Scenario: Daily cap (end to end, with a cap of one): the second
/// failing session of the day queues nothing and says nothing.
#[test]
fn the_daily_cap_stops_a_second_job_and_its_notice() {
    let home = mock_home();
    common::append_config(
        home.path(),
        &format!("{}max_jobs_per_day = 1\n", auto_config(40)),
    );
    let script = home.path().join("script.toml");
    std::fs::write(&script, FAILS_AFTER_A_TOOL).unwrap();
    let fail = || {
        let args = [
            "chat",
            "--mock-script",
            script.to_str().unwrap(),
            "-z",
            "go",
        ];
        stderr(&run(home.path(), &args))
    };
    assert!(fail().contains(QUEUED));
    let second = fail();
    assert!(!second.contains(QUEUED), "{second}");
    assert_eq!(jobs(home.path()).len(), 1);
}

/// Scenario: Evaluation tasks never evolve the harness: an `eval run`
/// task session that fails after a tool call queues no job (benchmark
/// prompts would otherwise tune the harness to the suite measuring it).
#[cfg(feature = "eval")]
#[test]
fn a_failing_eval_task_queues_no_job() {
    let home = mock_home();
    common::append_config(home.path(), &auto_config(40));
    let suite = home.path().join("suite");
    let task = suite.join("tasks/t1");
    std::fs::create_dir_all(&task).unwrap();
    std::fs::write(
        suite.join("suite.toml"),
        "name = \"auto\"\nversion = \"1\"\ndomain = \"accounting\"\n",
    )
    .unwrap();
    std::fs::write(
        task.join("task.toml"),
        "prompt = \"reconcile the ledger\"\n",
    )
    .unwrap();
    std::fs::write(
        task.join("mock.toml"),
        "[[rule]]\nstep = 0\n[[rule.tool_calls]]\nname = \"terminal\"\narguments = { command = \"echo hi\" }\n\n\
         [[rule]]\nstep = 1\nrepeat = true\nfault = \"auth_error\"\n",
    )
    .unwrap();
    let out = run(
        home.path(),
        &["eval", "run", suite.to_str().unwrap(), "--provider", "mock"],
    );
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let result = std::fs::read_dir(home.path().join("profiles/default/evals/runs"))
        .unwrap()
        .flatten()
        .map(|run| run.path().join("tasks/t1/1/result.json"))
        .find(|p| p.is_file())
        .expect("the task ran");
    let result: Value = serde_json::from_str(&std::fs::read_to_string(result).unwrap()).unwrap();
    assert_eq!(result["ok"], false, "the task failed: {result}");
    assert!(jobs(home.path()).is_empty(), "{:?}", jobs(home.path()));
    assert!(!stderr(&out).contains(QUEUED), "{}", stderr(&out));
}

/// The line interface says it as the user leaves.
#[cfg(unix)]
#[test]
fn the_line_interface_says_so_as_the_user_leaves() {
    use common::pty::{prompt, spawn};
    use expectrl::{Eof, Expect as _};
    let home = mock_home();
    common::append_config(home.path(), &auto_config(1));
    let mut p = spawn(home.path(), &[]);
    prompt(&mut p);
    p.send_line("reconcile the ledger").unwrap();
    p.expect("mock: reconcile the ledger").unwrap();
    prompt(&mut p);
    p.send_line("/exit").unwrap();
    p.expect(QUEUED).unwrap();
    p.expect(Eof).unwrap();
    let status = p.get_process_mut().wait().unwrap();
    assert!(format!("{status:?}").ends_with(", 0)"), "{status:?}");
    assert_eq!(jobs(home.path())[0]["trigger"], "auto");
}

/// A messaging chat is told when `/new` ends a qualifying session.
#[cfg(feature = "gateway")]
#[test]
fn a_chat_is_told_when_new_ends_a_session_that_queues_a_job() {
    use common::gateway::{Daemon, gateway_home_with, send, wait_said};
    let extra = format!("{}on_failure = false\n", auto_config(2));
    let home = gateway_home_with(TWO_ITERATIONS, &extra);
    let home = home.path();
    let daemon = Daemon::start(home);
    send(home, "default", "u1", "remember where the ledger is");
    wait_said(home, "default", "noted");
    send(home, "default", "u1", "/new");
    wait_said(home, "default", QUEUED);
    daemon.stop(home);
    let jobs = jobs(home);
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    assert_eq!(jobs[0]["trigger"], "auto");
}

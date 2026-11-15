//! `prometheus evolve`, `/evolve` and scheduled evolution jobs on the mock
//! provider (spec: harness-evolution, Opt-in per profile; On-demand and
//! scheduled triggers; Modes, budget and labels).

#![cfg(feature = "evolve")]

mod common;

use std::path::Path;

use common::{mock_home, ok, run, stderr, temp_home};
use serde_json::Value;

fn json(home: &Path, args: &[&str]) -> Value {
    let mut all = vec!["--json"];
    all.extend(args);
    serde_json::from_str(&ok(&run(home, &all))).unwrap()
}

fn enable(home: &Path) {
    ok(&run(home, &["config", "set", "evolve.enabled", "true"]));
}

/// Scenario: Disabled profile.
#[test]
fn a_disabled_profile_exits_78_and_writes_nothing() {
    let home = temp_home();
    let out = run(
        home.path(),
        &["evolve", "run", "--session", "01JABCDEFGHJKMNPQRSTVWXYZ0"],
    );
    assert_eq!(out.status.code(), Some(78), "{}", stderr(&out));
    assert!(
        stderr(&out).contains(
            "harness evolution is disabled for profile 'default'; set evolve.enabled = true"
        ),
        "{}",
        stderr(&out)
    );
    assert!(!home.path().join("profiles/default/evolve").exists());
}

/// Scenario: Invalid k.
#[test]
fn k_nine_is_a_usage_error() {
    let home = temp_home();
    enable(home.path());
    let out = run(
        home.path(),
        &[
            "evolve",
            "run",
            "--session",
            "01JABCDEFGHJKMNPQRSTVWXYZ0",
            "--k",
            "9",
        ],
    );
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("--k must be between 1 and 8"),
        "{}",
        stderr(&out)
    );
    assert!(!home.path().join("profiles/default/evolve").exists());
}

#[test]
fn a_task_file_job_is_queued_with_its_budget() {
    let home = temp_home();
    enable(home.path());
    let task = home.path().join("ledger.toml");
    std::fs::write(
        &task,
        "statement = \"sum the ledger\"\ndomain = \"accounting\"\n",
    )
    .unwrap();
    let out = ok(&run(
        home.path(),
        &[
            "evolve",
            "run",
            "--task",
            task.to_str().unwrap(),
            "--k",
            "3",
            "--mode",
            "both",
            "--detach",
        ],
    ));
    assert!(
        out.contains("parallel: 12 rollouts, 9 improver sessions"),
        "{out}"
    );
    assert!(
        out.contains("sequential: 12 rollouts, 9 improver sessions"),
        "{out}"
    );
    let jobs = json(home.path(), &["evolve", "list"]);
    assert_eq!(jobs[0]["trigger"], "cli");
    assert_eq!(jobs[0]["domain"], "accounting");
    assert_eq!(jobs[0]["mode"], "both");
    assert_eq!(jobs[0]["status"], "queued");
}

/// Scenario: Cron-triggered evolution.
#[test]
fn a_scheduled_evolve_job_queues_an_evolution_job_with_its_id() {
    let home = mock_home();
    enable(home.path());
    let tasks = home.path().join("profiles/default/tasks");
    std::fs::create_dir_all(&tasks).unwrap();
    std::fs::write(
        tasks.join("weekly-report.toml"),
        "statement = \"write the weekly report\"\n",
    )
    .unwrap();
    let created = json(
        home.path(),
        &[
            "cron",
            "add",
            "every 7d",
            "--evolve-task",
            "tasks/weekly-report.toml",
            "--name",
            "weekly",
            "--domain",
            "Weekly Reports",
            "--yes",
        ],
    );
    let cron_id = created["job"]["id"].as_str().unwrap().to_owned();
    let text = ok(&run(home.path(), &["cron", "run", "weekly"]));
    assert!(text.contains("evolution job evo_"), "{text}");
    let jobs = json(home.path(), &["evolve", "list"]);
    assert_eq!(jobs[0]["trigger"], "cron");
    assert_eq!(jobs[0]["cron_job_id"], cron_id.as_str());
    // The scheduled job's domain files the job (spec: experience-library,
    // Domain resolution for a job).
    assert_eq!(jobs[0]["domain"], "weekly-reports");
    let job = jobs[0]["id"].as_str().unwrap();
    let meta = std::fs::read_to_string(
        home.path()
            .join("profiles/default/evolve/jobs")
            .join(job)
            .join("task/task.toml"),
    )
    .unwrap();
    assert!(meta.contains("domain_source = \"cron\""), "{meta}");
    // No agent ran: no session was created.
    let sessions = json(home.path(), &["sessions", "list", "--all"]);
    assert_eq!(sessions["sessions"].as_array().unwrap().len(), 0);
}

/// Scenario: Slash command does not block.
#[cfg(unix)]
#[test]
fn slash_evolve_answers_with_the_job_id_without_a_model_call() {
    use common::pty::{exit, prompt, spawn};
    use expectrl::Expect as _;

    let home = mock_home();
    enable(home.path());
    let mut p = spawn(home.path(), &[]);
    prompt(&mut p);
    p.send_line("reconcile the ledger").unwrap();
    p.expect("mock: reconcile the ledger").unwrap();
    prompt(&mut p);
    let session = common::newest_session(home.path());
    let calls = || {
        let usage = json(home.path(), &["usage", "--by", "day"]);
        usage["total"]["calls"].as_u64().unwrap_or(0)
    };
    let before = calls();
    p.send_line("/evolve --k 1").unwrap();
    p.expect("job evo_").unwrap();
    p.expect("queued (k=1").unwrap();
    prompt(&mut p);
    assert_eq!(calls(), before, "the job does no work in the turn");
    p.send_line("after").unwrap();
    p.expect("mock: after").unwrap();
    prompt(&mut p);
    exit(p);
    let jobs = json(home.path(), &["evolve", "list"]);
    assert_eq!(jobs[0]["trigger"], "slash");
    assert_eq!(jobs[0]["source_session"], session.as_str());
    let trajectory = ok(&run(
        home.path(),
        &["trajectory", "show", &session, "--format", "jsonl"],
    ));
    let rebuilt = trajectory
        .lines()
        .filter(|l| l.contains("\"prompt.rebuilt\""))
        .count();
    assert_eq!(
        rebuilt, 1,
        "the session prompt was built once: {trajectory}"
    );
}

//! Scheduled jobs inside the real daemon, delivering through the
//! debug-build file adapter (spec: scheduled-jobs, Built-in trigger
//! inside the gateway daemon; Headless tick without a daemon; Delivery
//! targets; Durable delivery outbox; Model tool surface; Canonical
//! schedule echo and confirmation).

#![cfg(feature = "gateway")]

mod common;

use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};

use self::common::gateway::{Daemon, gateway_home_with, sent, wait};
use self::common::{ok, run};

const RULES: &str = "[[rule]]\nrepeat = true\ntext = \"default says hi\"\n";
const CRON: &str = "\n[cron]\ntick_interval_secs = 10\ntimezone = \"UTC\"\n";

fn json(home: &Path, args: &[&str]) -> Value {
    let mut all = vec!["--json"];
    all.extend(args);
    serde_json::from_str(&ok(&run(home, &all))).unwrap()
}

fn add(home: &Path, profile: &str, args: &[&str]) -> String {
    let mut all = vec!["-p", profile, "--json", "cron", "add"];
    all.extend(args);
    all.push("--yes");
    let doc: Value = serde_json::from_str(&ok(&run(home, &all))).unwrap();
    doc["job"]["id"].as_str().unwrap().to_owned()
}

fn scope_of(home: &Path, profile: &str) -> pmt_config::ProfileScope {
    let layout = pmt_config::Home::open(home).unwrap();
    let dir = layout
        .existing_profile(&pmt_config::ProfileName::parse(profile).unwrap())
        .unwrap();
    pmt_config::ProfileScope::open(
        dir,
        &pmt_config::Overrides::default(),
        pmt_config::Launch::Other,
    )
    .unwrap()
}

/// Makes job `id` of `profile` due now.
fn due_now(home: &Path, profile: &str, id: &str) {
    let store = pmt_store::Store::open(&scope_of(home, profile)).unwrap();
    let now = jiff::Timestamp::now().as_millisecond();
    store
        .cron_modify_job(id, |row| {
            row.next_run_at = Some(now - 1_000);
            Ok(())
        })
        .unwrap();
}

fn last_run(home: &Path, profile: &str, id: &str) -> Value {
    json(home, &["-p", profile, "cron", "runs", id])["runs"][0].clone()
}

fn wait_status(home: &Path, profile: &str, id: &str, done: &[&str]) -> Value {
    let mut run = Value::Null;
    wait("the run settles", Duration::from_mins(2), || {
        run = last_run(home, profile, id);
        run["status"].as_str().is_some_and(|s| done.contains(&s))
    });
    run
}

/// Scenario: Daemon present.
#[test]
fn a_tick_steps_aside_while_the_daemon_owns_scheduling() {
    let home = gateway_home_with(RULES, CRON);
    let daemon = Daemon::start(home.path());
    let mut lease = Value::Null;
    wait(
        "the daemon takes the tick lease",
        Duration::from_secs(30),
        || {
            lease = json(home.path(), &["cron", "status"])["lease"].clone();
            !lease.is_null()
        },
    );
    let pid = lease["pid"].as_u64().unwrap();
    let out = run(home.path(), &["cron", "tick"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        common::stdout(&out),
        format!("tick skipped: gateway daemon owns scheduling (pid {pid})\n")
    );
    daemon.stop(home.path());
}

/// Scenarios: Each profile ticked under its own scope; Profile isolation
/// of delivery.
#[test]
fn each_profile_runs_its_own_jobs_and_delivers_only_through_its_own_adapters() {
    let home = gateway_home_with(RULES, &format!("{CRON}\n[gateway]\nmultiplex = true\n"));
    ok(&run(
        home.path(),
        &["profile", "create", "work", "--no-alias"],
    ));
    std::fs::write(
        home.path().join("profiles/work/mock.toml"),
        "[[rule]]\nrepeat = true\ntext = \"work says hi\"\n",
    )
    .unwrap();
    for (key, value) in [
        ("model.provider", "mock"),
        ("model.default", "mock"),
        ("providers.mock.script", "mock.toml"),
        ("cron.tick_interval_secs", "10"),
        ("cron.preflight", "false"),
    ] {
        ok(&run(
            home.path(),
            &["-p", "work", "config", "set", key, value],
        ));
    }
    let home_job = add(
        home.path(),
        "default",
        &[
            "every 1h",
            "--prompt",
            "greet",
            "--name",
            "greeter",
            "--deliver",
            "test:chat-1",
        ],
    );
    let work_job = add(
        home.path(),
        "work",
        &["every 1h", "--prompt", "greet", "--deliver", "test"],
    );
    due_now(home.path(), "default", &home_job);
    due_now(home.path(), "work", &work_job);
    let daemon = Daemon::start(home.path());
    let home_run = wait_status(
        home.path(),
        "default",
        &home_job,
        &["ok", "delivery_failed", "failed"],
    );
    let work_run = wait_status(
        home.path(),
        "work",
        &work_job,
        &["ok", "delivery_failed", "failed"],
    );
    assert_eq!(home_run["status"], "ok", "{home_run}");
    assert_eq!(home_run["output"], "default says hi");
    assert_eq!(work_run["output"], "work says hi", "{work_run}");
    assert_eq!(work_run["status"], "delivery_failed");
    assert_eq!(work_run["error"], "test not configured for profile work");
    // The default profile's job ran in its store, the work job in work's.
    assert!(
        json(home.path(), &["cron", "runs", &home_job])["runs"]
            .as_array()
            .unwrap()
            .len()
            == 1
    );
    let missing = run(home.path(), &["cron", "runs", &work_job]);
    assert_eq!(missing.status.code(), Some(66));
    let delivered: Vec<Value> = sent(home.path(), "default")
        .into_iter()
        .filter(|m| m["kind"] == "send")
        .collect();
    assert_eq!(delivered.len(), 1, "{delivered:?}");
    assert_eq!(delivered[0]["chat"], "chat-1");
    assert_eq!(
        delivered[0]["text"],
        format!(
            "⏰ greeter\n\ndefault says hi\n\njob {home_job} · prometheus cron pause {home_job}"
        )
    );
    assert!(sent(home.path(), "work").is_empty());
    daemon.stop(home.path());
}

/// Scenarios: Tool create needs a confirmation round-trip; Created from
/// Telegram delivers back to Telegram (the file adapter's platform).
#[test]
fn a_job_the_agent_creates_from_a_chat_needs_confirmation_and_delivers_back_there() {
    let home = gateway_home_with(RULES, CRON);
    let scope = scope_of(home.path(), "default");
    let store = std::sync::Arc::new(pmt_store::Store::open(&scope).unwrap());
    let mut chat = pmt_store::NewSession::new(pmt_store::Source::Gateway);
    chat.origin = pmt_store::Origin::Gateway {
        platform: "test".into(),
        chat: "chat-9".into(),
        thread: Some("thread-3".into()),
        user: None,
    };
    let session = store.create_session(&chat).unwrap().id;
    let launch = pmt_core::env::LaunchEnv::default();
    let port = prometheus::cron_host::tool::port(&scope, &launch, &store, true);
    let args =
        json!({"action": "create", "schedule": "every 1h", "prompt": "status", "name": "status"});
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let first = runtime.block_on(port.call(session, args.clone()));
    let reply: Value = serde_json::from_str(&first.text_content()).unwrap();
    assert_eq!(reply["status"], "needs_confirmation", "{reply}");
    assert_eq!(reply["echo"]["canonical"], "every 1h");
    assert_eq!(reply["echo"]["next"].as_array().unwrap().len(), 3);
    assert!(
        store.cron_jobs().unwrap().is_empty(),
        "nothing saved before confirmation"
    );
    let mut confirmed = args.clone();
    confirmed["confirm_token"] = reply["confirm_token"].clone();
    let second = runtime.block_on(port.call(session, confirmed.clone()));
    let created: Value = serde_json::from_str(&second.text_content()).unwrap();
    assert_eq!(created["status"], "created", "{created}");
    assert_eq!(created["job"]["deliver"], json!(["origin"]));
    let reused = runtime.block_on(port.call(session, confirmed));
    assert_eq!(
        reused.status,
        pmt_tools::result::Status::Error,
        "a token is used once"
    );
    let id = created["job"]["id"].as_str().unwrap().to_owned();
    due_now(home.path(), "default", &id);
    let daemon = Daemon::start(home.path());
    wait_status(home.path(), "default", &id, &["ok", "delivery_failed"]);
    let delivered: Vec<Value> = sent(home.path(), "default")
        .into_iter()
        .filter(|m| m["kind"] == "send")
        .collect();
    assert_eq!(delivered.len(), 1, "{delivered:?}");
    assert_eq!(
        (&delivered[0]["chat"], &delivered[0]["thread"]),
        (&json!("chat-9"), &json!("thread-3"))
    );
    daemon.stop(home.path());
}

/// Scenario: Add from a Discord thread (the gateway path every adapter
/// shares, on the debug-build file adapter).
#[test]
fn a_job_added_with_slash_cron_in_a_thread_delivers_there() {
    let home = gateway_home_with(RULES, CRON);
    let daemon = Daemon::start(home.path());
    self::common::gateway::send_in(
        home.path(),
        "default",
        "u1",
        "chat-7",
        "thread-4",
        "/cron add \"every day at 18:00\" \"summarize today's commits\" --yes",
    );
    let mut jobs = Value::Null;
    wait("the job is created", Duration::from_mins(1), || {
        jobs = json(home.path(), &["cron", "list"])["jobs"].clone();
        jobs.as_array().is_some_and(|j| !j.is_empty())
    });
    let job = &jobs[0];
    assert_eq!(job["prompt"], "summarize today's commits");
    assert_eq!(job["schedule"], "cron 0 18 * * *");
    assert_eq!(job["deliver"], json!(["origin"]));
    let id = job["id"].as_str().unwrap().to_owned();
    self::common::gateway::wait_said(home.path(), "default", &format!("created job {id}"));
    due_now(home.path(), "default", &id);
    wait_status(home.path(), "default", &id, &["ok", "delivery_failed"]);
    let delivered: Vec<Value> = sent(home.path(), "default")
        .into_iter()
        .filter(|m| m["kind"] == "send" && m["text"].as_str().is_some_and(|t| t.starts_with("⏰")))
        .collect();
    assert_eq!(delivered.len(), 1, "{delivered:?}");
    assert_eq!(
        (&delivered[0]["chat"], &delivered[0]["thread"]),
        (&json!("chat-7"), &json!("thread-4"))
    );
    daemon.stop(home.path());
}

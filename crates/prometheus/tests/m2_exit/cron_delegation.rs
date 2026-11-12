//! Task 60.3: a scheduled job whose agent delegates to a subagent fires
//! once, and its delivery survives a daemon restart through the durable
//! outboxes (spec: scheduled-jobs, At-most-once occurrence firing; Built-in
//! trigger inside the gateway daemon; Durable delivery outbox;
//! subagent-delegation, Background and synchronous dispatch; Durable
//! background completions; messaging-gateway, Durable outbound outbox).
//! The platform is down when the run ends: the job's delivery is handed
//! from `cron_deliveries` to the gateway's outbox, where it waits; the
//! daemon restarts, the platform comes back, and the queued message is
//! sent exactly once while the occurrence is never fired again.

use std::path::Path;
use std::time::Duration;

use serde_json::Value;

use crate::common::gateway::{Daemon, adapter_dir, gateway_home_with, sent, wait};
use crate::common::{ok, run};

const RULES: &str = r#"
[[rule]]
user_regex = "check the nightly build"
[[rule.tool_calls]]
name = "delegate_task"
arguments = { tasks = [{ goal = "read the nightly build log" }] }

[[rule]]
user_regex = "read the nightly build log"
latency_ms = 500
text = "the nightly build is green"

[[rule]]
tool_result_regex = "the nightly build is green"
text = "Nightly report: the subagent says the build is green."

[[rule]]
repeat = true
text = "unexpected"
"#;

const CONFIG: &str = "\n[cron]\ntick_interval_secs = 10\ntimezone = \"UTC\"\npreflight = false\n";

fn db(home: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open_with_flags(
        home.join("profiles/default/state.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap()
}

fn scope(home: &Path) -> pmt_config::ProfileScope {
    let layout = pmt_config::Home::open(home).unwrap();
    let dir = layout
        .existing_profile(&pmt_config::ProfileName::parse("default").unwrap())
        .unwrap();
    pmt_config::ProfileScope::open(
        dir,
        &pmt_config::Overrides::default(),
        pmt_config::Launch::Other,
    )
    .unwrap()
}

fn json(home: &Path, args: &[&str]) -> Value {
    let mut all = vec!["--json"];
    all.extend(args);
    serde_json::from_str(&ok(&run(home, &all))).unwrap()
}

/// `(state, last error)` of every row of the job's delivery outbox
/// (`cron_deliveries`) or of the gateway's (`gateway_outbox`), which
/// a job's chat delivery is handed to.
fn rows(home: &Path, table: &str) -> Vec<(String, String)> {
    let db = db(home);
    let mut stmt = db
        .prepare(&format!(
            "SELECT state, coalesce({error}, '') FROM {table} ORDER BY created_at",
            error = if table == "gateway_outbox" {
                "last_error"
            } else {
                "error"
            }
        ))
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

/// `(status, parent session, result)` of every subagent row.
fn subagents(home: &Path) -> Vec<(String, String, String)> {
    let db = db(home);
    let mut stmt = db
        .prepare("SELECT status, parent_session_id, coalesce(result, '') FROM subagents")
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn delivered(home: &Path) -> Vec<String> {
    sent(home, "default")
        .iter()
        .filter(|m| m["kind"] == "send")
        .map(|m| m["text"].as_str().unwrap_or_default().to_owned())
        .collect()
}

#[test]
fn a_delegating_job_fires_once_and_its_queued_delivery_survives_a_restart() {
    let home = gateway_home_with(RULES, CONFIG);
    let home = home.path();
    let added = json(
        home,
        &[
            "cron",
            "add",
            "every 1h",
            "--prompt",
            "check the nightly build",
            "--name",
            "nightly",
            "--deliver",
            "test:ops",
            "--yes",
        ],
    );
    let job = added["job"]["id"].as_str().unwrap().to_owned();
    // The platform is down when the run delivers.
    let down = adapter_dir(home).join("default/down");
    std::fs::create_dir_all(down.parent().unwrap()).unwrap();
    std::fs::write(&down, "").unwrap();
    let store = pmt_store::Store::open(&scope(home)).unwrap();
    let now = jiff::Timestamp::now().as_millisecond();
    store
        .cron_modify_job(&job, |row| {
            row.next_run_at = Some(now - 1_000);
            Ok(())
        })
        .unwrap();

    let daemon = Daemon::start(home);
    wait(
        "the delivery waits in the outbox after a failed attempt",
        Duration::from_mins(2),
        || {
            rows(home, "gateway_outbox")
                .first()
                .is_some_and(|(state, error)| state == "queued" && error.contains("platform is down"))
                // The job's own outbox has handed the message over.
                && rows(home, "cron_deliveries") == [("sent".to_owned(), String::new())]
        },
    );
    let children = subagents(home);
    assert_eq!(children.len(), 1, "{children:?}");
    assert_eq!(children[0].0, "completed", "{children:?}");
    assert!(children[0].2.contains("the nightly build is green"));
    let runs = json(home, &["cron", "runs", &job])["runs"].clone();
    assert_eq!(runs.as_array().unwrap().len(), 1, "{runs}");
    assert_eq!(
        runs[0]["output"],
        "Nightly report: the subagent says the build is green."
    );
    assert_eq!(runs[0]["session_id"], children[0].1, "{runs}");
    assert!(delivered(home).is_empty());
    assert_eq!(daemon.stop(home), Some(0));

    // The platform is back; a new daemon sends what the old one queued.
    std::fs::remove_file(&down).unwrap();
    let daemon = Daemon::start(home);
    wait(
        "the queued delivery is sent",
        Duration::from_mins(2),
        || !delivered(home).is_empty(),
    );
    // Two more ticks: nothing fires or sends again.
    std::thread::sleep(Duration::from_secs(21));
    let texts = delivered(home);
    assert_eq!(texts.len(), 1, "{texts:?}");
    assert_eq!(
        texts[0],
        format!(
            "⏰ nightly\n\nNightly report: the subagent says the build is green.\n\n\
             job {job} · prometheus cron pause {job}"
        )
    );
    assert_eq!(rows(home, "gateway_outbox")[0].0, "sent");
    assert_eq!(
        rows(home, "cron_deliveries"),
        [("sent".to_owned(), String::new())]
    );
    let runs = json(home, &["cron", "runs", &job])["runs"].clone();
    assert_eq!(runs.as_array().unwrap().len(), 1, "{runs}");
    assert_eq!(runs[0]["status"], "ok", "{runs}");
    assert_eq!(subagents(home).len(), 1);
    assert_eq!(daemon.stop(home), Some(0));
}

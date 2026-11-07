//! The M2 job upgrades through `prometheus cron` and the `cronjob` tool,
//! against the mock provider (spec: scheduled-jobs, Per-job working
//! directory; Model pinning and cron fleet default; Create paused with an
//! auditable reason; Failure incidents with alert-once and acknowledge;
//! Per-job notepad; Complete cronjob tool field set; Asynchronous
//! tool-initiated runs with per-run context).

mod common;

use std::path::Path;

use common::{mock_home, ok, run, stderr, stdout};
use serde_json::{Value, json};

fn cron_home(rules: &str) -> tempfile::TempDir {
    let home = mock_home();
    std::fs::write(home.path().join("profiles/default/mock.toml"), rules).unwrap();
    common::append_config(
        home.path(),
        "\n[providers.mock]\nscript = \"mock.toml\"\n\n[cron]\ntimezone = \"UTC\"\n",
    );
    home
}

fn json(home: &Path, args: &[&str]) -> Value {
    let mut all = vec!["--json"];
    all.extend(args);
    serde_json::from_str(&ok(&run(home, &all))).unwrap()
}

fn add(home: &Path, args: &[&str]) -> String {
    let mut all = vec!["cron", "add"];
    all.extend(args);
    all.push("--yes");
    json(home, &all)["job"]["id"].as_str().unwrap().to_owned()
}

/// Runs `job` now and returns its run record.
fn run_job(home: &Path, job: &str) -> Value {
    let out = run(home, &["cron", "run", job]);
    let id = stdout(&out)
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .trim_end_matches(':')
        .to_owned();
    assert!(id.starts_with("run_"), "{}{}", stdout(&out), stderr(&out));
    json(home, &["cron", "show-run", &id])
}

fn scope(home: &Path) -> pmt_config::ProfileScope {
    use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
    let home = Home::open(home).unwrap();
    let profile = home
        .existing_profile(&ProfileName::parse("default").unwrap())
        .unwrap();
    ProfileScope::open(profile, &Overrides::default(), Launch::Other).unwrap()
}

/// Scenario: Relative workdir refused.
#[test]
fn a_relative_workdir_is_refused() {
    let home = cron_home("[[rule]]\nrepeat = true\ntext = \"x\"\n");
    let out = run(
        home.path(),
        &[
            "cron",
            "add",
            "every 1d",
            "--prompt",
            "x",
            "--workdir",
            "../acme",
            "--yes",
        ],
    );
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(stderr(&out).contains("workdir must be an existing absolute directory"));
    assert_eq!(
        json(home.path(), &["cron", "list", "--all"])["jobs"],
        json!([])
    );
}

/// Scenario: Project job loads its context files.
#[test]
fn a_project_job_loads_its_context_files_and_works_there() {
    let project = tempfile::tempdir().unwrap();
    let dir = project.path().canonicalize().unwrap();
    std::fs::write(dir.join("AGENTS.md"), "ACME-PROJECT-RULES\n").unwrap();
    let rules = "[[rule]]\nuser = \"where\"\n[[rule.tool_calls]]\nname = \"terminal\"\n\
                 arguments = { command = \"pwd\" }\n\n[[rule]]\nrepeat = true\ntext = \"here\"\n";
    let home = cron_home(rules);
    let workdir = dir.to_string_lossy().into_owned();
    let id = add(
        home.path(),
        &[
            "every 1d",
            "--prompt",
            "where am I",
            "--workdir",
            &workdir,
            "--toolsets",
            "terminal",
        ],
    );
    let run = run_job(home.path(), &id);
    assert_eq!(run["status"], "ok", "{run}");
    let session: pmt_core::id::SessionId = run["session_id"].as_str().unwrap().parse().unwrap();
    let store = pmt_store::Store::open(&scope(home.path())).unwrap();
    let prompt = store.session_prompt(session).unwrap().unwrap();
    let system: String = prompt.tiers.into_iter().map(|t| t.text).collect();
    assert!(system.contains("ACME-PROJECT-RULES"), "{system}");
    let messages = common::session_messages(home.path(), &session.to_string());
    let tool = messages
        .iter()
        .find(|m| m["role"] == "tool")
        .expect("a terminal result");
    assert!(tool.to_string().contains(&workdir), "{tool}");
    // A detached job loads no project context.
    let id = add(home.path(), &["every 1d", "--prompt", "plain"]);
    let run = run_job(home.path(), &id);
    let session: pmt_core::id::SessionId = run["session_id"].as_str().unwrap().parse().unwrap();
    let prompt = store.session_prompt(session).unwrap().unwrap();
    let system: String = prompt.tiers.into_iter().map(|t| t.text).collect();
    assert!(!system.contains("ACME-PROJECT-RULES"));
}

const BY_MODEL: &str = "[[rule]]\nmodel = \"small-model\"\nrepeat = true\ntext = \"used small\"\n\n\
     [[rule]]\nmodel = \"big-model\"\nrepeat = true\ntext = \"used big\"\n\n\
     [[rule]]\nrepeat = true\ntext = \"used other\"\n";

/// Scenarios: Unpinned jobs follow the cron default; Pin freezes the
/// current main model.
#[test]
fn unpinned_jobs_follow_the_cron_default_and_pins_freeze() {
    let home = cron_home(BY_MODEL);
    ok(&run(
        home.path(),
        &["config", "set", "model.default", "big-model"],
    ));
    let unpinned = add(home.path(), &["every 1d", "--prompt", "report"]);
    let pinned = add(
        home.path(),
        &["every 1d", "--prompt", "report", "--name", "nightly"],
    );
    ok(&run(home.path(), &["cron", "edit", "nightly", "--pin"]));
    ok(&run(
        home.path(),
        &["config", "set", "cron.model", "small-model"],
    ));
    ok(&run(
        home.path(),
        &["config", "set", "model.default", "other-model"],
    ));
    assert_eq!(run_job(home.path(), &unpinned)["output"], "used small");
    assert_eq!(run_job(home.path(), &pinned)["output"], "used big");
    let jobs = json(home.path(), &["cron", "list"])["jobs"].clone();
    let flags: Vec<_> = jobs
        .as_array()
        .unwrap()
        .iter()
        .map(|j| j["pinned"].clone())
        .collect();
    assert_eq!(flags, [json!(false), json!(true)]);
    ok(&run(home.path(), &["cron", "edit", "nightly", "--unpin"]));
    assert_eq!(run_job(home.path(), &pinned)["output"], "used small");
}

/// Scenario: Canary job never fires before review.
#[test]
fn a_job_added_paused_waits_for_review() {
    let home = cron_home("[[rule]]\nrepeat = true\ntext = \"x\"\n");
    let id = add(home.path(), &["every 1h", "--prompt", "digest", "--paused"]);
    ok(&run(home.path(), &["cron", "tick"]));
    let listed = ok(&run(home.path(), &["cron", "list"]));
    assert!(
        listed.contains("paused: Created paused; awaiting operator approval."),
        "{listed}"
    );
    let runs = json(home.path(), &["cron", "runs", &id])["runs"].clone();
    assert_eq!(runs, json!([]));
    let job = &json(home.path(), &["cron", "list"])["jobs"][0];
    assert_eq!(
        (&job["state"], &job["next_run_at"]),
        (&json!("paused"), &Value::Null)
    );
    let out = run(
        home.path(),
        &[
            "cron",
            "add",
            "every 1h",
            "--prompt",
            "x",
            "--paused-reason",
            "why",
            "--yes",
        ],
    );
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
}

/// Incidents through the CLI: a failing job opens one, `ack` closes it.
#[test]
fn incidents_are_listed_and_acknowledged() {
    let home = cron_home(
        "[[rule]]\nrepeat = true\ntext = \"[CRON_FAILURE]\\nexport exited: disk full\"\n",
    );
    let id = add(home.path(), &["every 1h", "--prompt", "export"]);
    let failed = run_job(home.path(), &id);
    assert_eq!(failed["status"], "failed");
    assert_eq!(failed["error"], "export exited: disk full");
    let listed = json(home.path(), &["cron", "incidents", "--job", &id]);
    let incident = &listed["incidents"][0];
    assert_eq!(
        (&incident["state"], &incident["error"]),
        (&json!("alerted"), &json!("export exited: disk full"))
    );
    let incident_id = incident["id"].as_str().unwrap();
    ok(&run(
        home.path(),
        &["cron", "incidents", "ack", incident_id],
    ));
    let closed = json(home.path(), &["cron", "incidents", "--state", "closed"]);
    assert_eq!(closed["incidents"].as_array().unwrap().len(), 1);
    let listed = ok(&run(home.path(), &["cron", "list"]));
    assert!(listed.contains("failing 1 run in a row"), "{listed}");
    let missing = run(home.path(), &["cron", "incidents", "ack", "inc_nope"]);
    assert_eq!(missing.status.code(), Some(66));
}

/// Scenario: Cursor carried across runs.
#[test]
fn a_notepad_value_reaches_the_next_run() {
    let home = cron_home("[[rule]]\nrepeat = true\ntext = \"ok\"\n");
    let id = add(
        home.path(),
        &["every 1h", "--prompt", "read the feed", "--name", "feed"],
    );
    ok(&run(
        home.path(),
        &["cron", "notepad", "feed", "set", "last_id", "4812"],
    ));
    assert_eq!(
        ok(&run(
            home.path(),
            &["cron", "notepad", "feed", "get", "last_id"]
        )),
        "4812\n"
    );
    let done = run_job(home.path(), &id);
    let messages = common::session_messages(home.path(), done["session_id"].as_str().unwrap());
    let user = messages
        .iter()
        .find(|m| m["role"] == "user")
        .unwrap()
        .to_string();
    assert!(
        user.contains("## Job notepad") && user.contains("last_id: 4812"),
        "{user}"
    );
    let big = "x".repeat(17 * 1024);
    let out = run(
        home.path(),
        &["cron", "notepad", "feed", "set", "big", &big],
    );
    assert_eq!(out.status.code(), Some(65));
    assert!(stderr(&out).contains("notepad limit exceeded"));
}

fn tool_call(
    port: &std::sync::Arc<dyn pmt_agent::tools::CronPort>,
    session: pmt_core::id::SessionId,
    args: Value,
) -> Value {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let result = runtime.block_on(port.call(session, args));
    serde_json::from_str(&result.text_content())
        .unwrap_or_else(|_| json!({"error": result.text_content()}))
}

/// Scenario: Timezone pin through the tool.
#[test]
fn the_tool_pins_a_timezone() {
    let home = cron_home("[[rule]]\nrepeat = true\ntext = \"ok\"\n");
    let scope = scope(home.path());
    let store = std::sync::Arc::new(pmt_store::Store::open(&scope).unwrap());
    let session = store
        .create_session(&pmt_store::NewSession::new(pmt_store::Source::Cli))
        .unwrap()
        .id;
    let port = prometheus::cron_host::tool::port(
        &scope,
        &pmt_core::env::LaunchEnv::default(),
        &store,
        true,
    );
    let reply = tool_call(
        &port,
        session,
        json!({"action": "create", "schedule": "0 9 * * 1-5",
        "timezone": "Asia/Hong_Kong", "prompt": "standup"}),
    );
    assert_eq!(reply["status"], "needs_confirmation", "{reply}");
    let summary = reply["summary"].as_str().unwrap();
    assert!(summary.contains("(Asia/Hong_Kong)"), "{summary}");
    assert!(summary.contains("09:00"), "{summary}");
}

/// Scenarios: Agent keeps working while the job runs; Per-run context is
/// not persisted.
#[test]
fn a_tool_run_starts_in_the_background_and_reports_back() {
    let home = cron_home("[[rule]]\nuser = \"focus on EU\"\nrepeat = true\ntext = \"EU digest\"\n");
    let id = add(
        home.path(),
        &["every 1d", "--prompt", "digest", "--name", "digest"],
    );
    let scope = scope(home.path());
    let store = std::sync::Arc::new(pmt_store::Store::open(&scope).unwrap());
    let session = store
        .create_session(&pmt_store::NewSession::new(pmt_store::Source::Cli))
        .unwrap()
        .id;
    let port = prometheus::cron_host::tool::port(
        &scope,
        &pmt_core::env::LaunchEnv::default(),
        &store,
        true,
    );
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let reply = runtime.block_on(async {
        let result = port
            .call(
                session,
                json!({"action": "run", "job_id": "digest", "prompt": "focus on EU"}),
            )
            .await;
        let reply: Value = serde_json::from_str(&result.text_content()).unwrap();
        // Wait (inside the runtime that runs it) for the outcome notice.
        let deadline = std::time::Instant::now() + std::time::Duration::from_mins(2);
        loop {
            let messages = store
                .messages(session, pmt_store::Visibility::Active)
                .unwrap();
            if messages.iter().any(|m| {
                m.content
                    .as_deref()
                    .is_some_and(|c| c.contains("[cron run finished]"))
            }) {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "no outcome notice");
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        reply
    });
    assert_eq!(reply["status"], "started", "{reply}");
    let run_id = reply["run_id"].as_str().unwrap();
    let run = json(home.path(), &["cron", "show-run", run_id]);
    assert_eq!(run["output"], "EU digest");
    let notice = store
        .messages(session, pmt_store::Visibility::Active)
        .unwrap()
        .into_iter()
        .filter_map(|m| m.content)
        .find(|c| c.contains("[cron run finished]"))
        .unwrap();
    assert!(
        notice.contains(run_id) && notice.contains("EU digest"),
        "{notice}"
    );
    let job = &json(home.path(), &["cron", "list"])["jobs"][0];
    assert_eq!((&job["id"], &job["prompt"]), (&json!(id), &json!("digest")));
}

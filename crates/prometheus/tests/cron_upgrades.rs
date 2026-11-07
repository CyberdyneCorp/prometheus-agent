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

/// Scenario: Cursor carried across runs, with the cursor written by the
/// run itself: its sandboxed terminal runs `prometheus cron notepad`.
#[test]
fn a_run_writes_its_notepad_from_its_terminal() {
    let rules = "[[rule]]\nuser = \"read the feed\"\n[[rule.tool_calls]]\nname = \"terminal\"\n\
                 arguments = { command = \"prometheus cron notepad feed set last_id 4812\" }\n\n\
                 [[rule]]\nrepeat = true\ntext = \"cursor saved\"\n";
    let home = cron_home(rules);
    let id = add(
        home.path(),
        &[
            "every 1h",
            "--prompt",
            "read the feed",
            "--name",
            "feed",
            "--toolsets",
            "terminal",
        ],
    );
    let first = run_job(home.path(), &id);
    assert_eq!(first["status"], "ok", "{first}");
    let messages = common::session_messages(home.path(), first["session_id"].as_str().unwrap());
    let tool = messages
        .iter()
        .find(|m| m["role"] == "tool")
        .expect("a terminal result")
        .to_string();
    assert!(!tool.contains("not permitted"), "{tool}");
    assert_eq!(
        ok(&run(
            home.path(),
            &["cron", "notepad", "feed", "get", "last_id"]
        )),
        "4812\n"
    );
    let second = run_job(home.path(), &id);
    let messages = common::session_messages(home.path(), second["session_id"].as_str().unwrap());
    let user = messages
        .iter()
        .find(|m| m["role"] == "user")
        .unwrap()
        .to_string();
    assert!(
        user.contains("## Job notepad") && user.contains("last_id: 4812"),
        "{user}"
    );
}

/// A run's terminal reaches only its own job's notepad, and the notepad
/// limits hold there too.
#[test]
fn a_run_cannot_write_another_jobs_notepad() {
    let rules = "[[rule]]\nuser = \"other\"\n[[rule.tool_calls]]\nname = \"terminal\"\n\
                 arguments = { command = \"prometheus cron notepad feed set stolen 1; echo rc=$?\" }\n\n\
                 [[rule]]\nrepeat = true\ntext = \"done\"\n";
    let home = cron_home(rules);
    add(
        home.path(),
        &["every 1h", "--prompt", "x", "--name", "feed"],
    );
    let id = add(
        home.path(),
        &[
            "every 1h",
            "--prompt",
            "other",
            "--name",
            "other",
            "--toolsets",
            "terminal",
        ],
    );
    let done = run_job(home.path(), &id);
    let messages = common::session_messages(home.path(), done["session_id"].as_str().unwrap());
    let tool = messages
        .iter()
        .find(|m| m["role"] == "tool")
        .expect("a terminal result")
        .to_string();
    assert!(
        tool.contains("edits only its own job's notepad") && tool.contains("rc=66"),
        "{tool}"
    );
    assert_eq!(
        ok(&run(home.path(), &["cron", "notepad", "feed", "list"])),
        ""
    );
}

/// Scenario: Provider key cannot reach a script, through the profile's
/// real catalog: a provider credential and a gateway-owned token (a
/// built-in platform's default secret, named nowhere in the config) are
/// both refused; an ordinary secret is accepted.
#[test]
fn provider_and_gateway_secrets_cannot_reach_a_script() {
    let home = cron_home("[[rule]]\nrepeat = true\ntext = \"ok\"\n");
    let scripts = home.path().join("profiles/default/scripts");
    std::fs::create_dir_all(&scripts).unwrap();
    std::fs::write(scripts.join("poll.sh"), "echo hi\n").unwrap();
    let add_with = |secret: &str| {
        run(
            home.path(),
            &[
                "cron",
                "add",
                "every 1h",
                "--prompt",
                "x",
                "--script",
                "poll.sh",
                "--script-secret",
                secret,
                "--yes",
            ],
        )
    };
    for reserved in ["OPENAI_API_KEY", "TELEGRAM_BOT_TOKEN"] {
        let out = add_with(reserved);
        assert_eq!(out.status.code(), Some(65), "{}", stderr(&out));
        assert!(
            stderr(&out).contains(&format!(
                "secret '{reserved}' cannot be passed to cron scripts"
            )),
            "{}",
            stderr(&out)
        );
    }
    assert_eq!(
        json(home.path(), &["cron", "list", "--all"])["jobs"],
        json!([])
    );
    ok(&add_with("FEED_TOKEN"));
}

/// A loopback HTTP server answering every GET with `body`.
fn page_server(body: &'static str) -> u16 {
    use std::io::{Read as _, Write as _};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request);
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    port
}

/// Scenario: Unchanged page is free, with the page fetched under the
/// profile's egress rules: a private address is refused (the agent sets
/// `monitor_url`) until `security.egress.allow_private` admits it; then
/// the first tick runs the agent and an unchanged page skips the model.
#[test]
fn a_monitored_url_obeys_the_egress_destination_policy() {
    let home = cron_home("[[rule]]\nrepeat = true\ntext = \"page changed\"\n");
    let url = format!("http://127.0.0.1:{}/feed", page_server("v1"));
    let id = add(
        home.path(),
        &["every 1h", "--prompt", "watch", "--monitor-url", &url],
    );
    let refused = run_job(home.path(), &id);
    assert_eq!(refused["status"], "failed", "{refused}");
    assert!(
        refused["error"]
            .as_str()
            .is_some_and(|e| e.contains("is a private address")),
        "{refused}"
    );
    ok(&run(
        home.path(),
        &["config", "set", "security.egress.allow_private", "true"],
    ));
    let first = run_job(home.path(), &id);
    assert_eq!(first["status"], "ok", "{first}");
    let second = run_job(home.path(), &id);
    assert_eq!(second["status"], "silent", "{second}");
    assert!(second.to_string().contains("no_change"), "{second}");
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

/// Scenario: Laptop wakes before Wi-Fi, against a real provider transport:
/// the model's endpoint refuses connections, so the scheduled run fails
/// before any model response; it is re-run automatically (300 s later,
/// `retry_of` the failed run) instead of sending a failure notice.
#[test]
fn an_unreachable_provider_schedules_a_re_run() {
    let home = common::temp_home();
    ok(&run(home.path(), &["config", "path"]));
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    std::fs::write(
        home.path().join("profiles/default/config.toml"),
        format!(
            "[model]\nprovider = \"acme\"\ndefault = \"r1\"\n\n\
             [providers.acme]\nbase_url = \"http://127.0.0.1:{port}/v1\"\n\
             wire_format = \"chat_completions\"\nauth = \"none\"\nmax_retries = 0\n\n\
             [agent]\napi_max_retries = 1\nauto_recovery_cycles = 0\n\n\
             [budgets]\nallow_unpriced = true\n\n[cron]\ntimezone = \"UTC\"\n"
        ),
    )
    .unwrap();
    let id = add(home.path(), &["every 1d", "--prompt", "brief"]);
    let store = pmt_store::Store::open(&scope(home.path())).unwrap();
    let now = jiff::Timestamp::now().as_millisecond();
    store
        .cron_modify_job(&id, |row| {
            row.next_run_at = Some(now - 1_000);
            Ok(())
        })
        .unwrap();
    ok(&run(home.path(), &["cron", "tick"]));
    let runs = json(home.path(), &["cron", "runs", &id])["runs"].clone();
    let failed = &runs[0];
    assert_eq!(
        (&failed["trigger"], &failed["status"]),
        (&json!("schedule"), &json!("failed")),
        "{runs}"
    );
    assert!(
        failed["error"]
            .as_str()
            .is_some_and(|e| e.contains("network error")),
        "{failed}"
    );
    let row = store.cron_job(&id).unwrap().unwrap();
    let retry_at = row.retry_at.expect("a re-run is scheduled");
    let wait = retry_at - failed_ended(failed);
    assert!((299_000..=301_000).contains(&wait), "re-run in {wait} ms");
    assert_eq!(row.retry_of.as_deref(), failed["run_id"].as_str());
    let run_id = failed["run_id"].as_str().unwrap();
    assert!(
        store.cron_deliveries_of(run_id).unwrap().is_empty(),
        "no failure notice"
    );
}

/// When run `run` (a `cron runs` entry) ended, in Unix ms.
fn failed_ended(run: &Value) -> i64 {
    run["ended_at"]
        .as_str()
        .unwrap()
        .parse::<jiff::Timestamp>()
        .unwrap()
        .as_millisecond()
}

/// Scenario: Tool workdir outside the session's workspace refused.
#[test]
fn a_tool_workdir_outside_the_sessions_workspace_is_refused() {
    let home = cron_home("[[rule]]\nrepeat = true\ntext = \"ok\"\n");
    let workspace = home.path().join("acme");
    std::fs::create_dir_all(workspace.join("docs")).unwrap();
    let scope = scope(home.path());
    let store = std::sync::Arc::new(pmt_store::Store::open(&scope).unwrap());
    let mut new = pmt_store::NewSession::new(pmt_store::Source::Cli);
    new.cwd = Some(workspace.to_string_lossy().into_owned());
    let session = store.create_session(&new).unwrap().id;
    let port = prometheus::cron_host::tool::port(
        &scope,
        &pmt_core::env::LaunchEnv::default(),
        &store,
        true,
    );
    let create = |workdir: &Path, token: Option<&str>| {
        let mut args = json!({"action": "create", "schedule": "every 1d", "prompt": "x",
            "workdir": workdir.to_string_lossy()});
        if let Some(token) = token {
            args["confirm_token"] = json!(token);
        }
        tool_call(&port, session, args)
    };
    let outside = home.path().canonicalize().unwrap();
    for token in [None, Some("forged")] {
        let reply = create(&outside, token);
        let error = reply["error"].as_str().unwrap_or_default();
        assert!(
            error.contains(&format!(
                "workdir {} is outside this session's workspace",
                outside.display()
            )),
            "{reply}"
        );
    }
    assert!(store.cron_jobs().unwrap().is_empty());
    let inside = create(&workspace.join("docs"), None);
    assert_eq!(inside["status"], "needs_confirmation", "{inside}");
}

/// Scenario: Sub-hourly job does not alert every tick, with the provider's
/// own signal: every credential of the profile's only provider (no
/// fallback chain) is benched for 5 hours, so the run fails before any
/// request, the job is held until the bench ends, and a later tick runs
/// nothing.
#[test]
fn a_benched_provider_holds_a_sub_hourly_job() {
    let home = common::temp_home();
    ok(&run(home.path(), &["config", "path"]));
    std::fs::write(
        home.path().join("profiles/default/config.toml"),
        "[model]\nprovider = \"openrouter\"\ndefault = \"x/y\"\n\n\
         [budgets]\nallow_unpriced = true\n\n[cron]\ntimezone = \"UTC\"\n",
    )
    .unwrap();
    ok(&common::run_with_stdin(
        home.path(),
        &["auth", "add", "openrouter"],
        "sk-or-v1-0123456789abcdef",
    ));
    let id = add(home.path(), &["every 15m", "--prompt", "brief"]);
    let store = pmt_store::Store::open(&scope(home.path())).unwrap();
    let now = jiff::Timestamp::now();
    let until = jiff::Timestamp::from_second(now.as_second() + 5 * 3600).unwrap();
    store
        .bench_credential(
            "openrouter",
            "secret:OPENROUTER_API_KEY",
            Some(until),
            Some("rate_limit"),
        )
        .unwrap();
    let due = |store: &pmt_store::Store| {
        store
            .cron_modify_job(&id, |row| {
                row.next_run_at = Some(jiff::Timestamp::now().as_millisecond() - 1_000);
                Ok(())
            })
            .unwrap();
    };
    due(&store);
    ok(&run(home.path(), &["cron", "tick"]));
    let runs = json(home.path(), &["cron", "runs", &id])["runs"].clone();
    assert_eq!(runs.as_array().unwrap().len(), 1, "{runs}");
    assert_eq!(runs[0]["status"], "failed", "{runs}");
    assert!(
        runs[0]["error"]
            .as_str()
            .is_some_and(|e| e.starts_with("usage window closed until")),
        "{runs}"
    );
    let row = store.cron_job(&id).unwrap().unwrap();
    assert_eq!(row.quota_hold_until, Some(until.as_millisecond()));
    // The single held notice goes to messaging targets (a `local` job
    // records it only); the scripted scheduler test counts it.
    due(&store);
    ok(&run(home.path(), &["cron", "tick"]));
    let runs = json(home.path(), &["cron", "runs", &id])["runs"].clone();
    assert_eq!(runs.as_array().unwrap().len(), 1, "held: {runs}");
}

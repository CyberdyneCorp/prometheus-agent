//! The Runs API of a running daemon (spec: api-server, Runs API
//! lifecycle; Run idempotency ledger; Run events stream; Run controls):
//! durable run records across restarts, the event stream and its
//! reconnects, approvals as pending controls with execution generations,
//! stop, and runs a crashed daemon left `unknown`.

#![cfg(feature = "gateway")]

mod common;

use std::path::Path;

use serde_json::{Value, json};

use self::common::api::{self, Request, run_until, run_until_terminal};
use self::common::{append_config, daemon, ok, run};

fn script(home: &Path, rules: &str) {
    let path = home.join("api-script.toml");
    std::fs::write(&path, rules).unwrap();
    append_config(
        home,
        &format!(
            "\n[providers.mock]\nscript = {:?}\n",
            path.to_string_lossy()
        ),
    );
}

fn create(port: u16, body: &Value) -> api::Answer {
    Request::new("POST", "/v1/runs")
        .auth("default")
        .json(body)
        .send(port)
}

fn run_id(answer: &api::Answer) -> String {
    assert_eq!(answer.status, 202, "{}", answer.body);
    answer.json()["run_id"].as_str().unwrap().to_owned()
}

fn control(port: u16, id: &str, action: &str, body: &Value) -> api::Answer {
    Request::new("POST", &format!("/v1/runs/{id}/{action}"))
        .auth("default")
        .json(body)
        .send(port)
}

fn events(id: &str, last: Option<&str>) -> Request<'static> {
    let path: &'static str = Box::leak(format!("/v1/runs/{id}/events").into_boxed_str());
    let request = Request::new("GET", path).auth("default");
    match last {
        Some(last) => request.header("Last-Event-ID", last),
        None => request,
    }
}

/// Scenario: Replay after restart.
#[test]
fn an_idempotent_run_is_replayed_after_a_restart() {
    let home = api::home("");
    let body = json!({"input": "ticket work"});
    let id = {
        let _daemon = daemon::start(home.path(), &[]);
        let port = api::port(home.path());
        let first = Request::new("POST", "/v1/runs")
            .auth("default")
            .header("Idempotency-Key", "ticket-123")
            .json(&body)
            .send(port);
        let id = run_id(&first);
        assert!(id.starts_with("run_"));
        let done = run_until_terminal(port, "default", &id);
        assert_eq!(done["status"], "completed");
        assert_eq!(done["output"], "mock: ticket work");
        for key in ["cache_read_tokens", "cache_write_tokens"] {
            assert!(done["usage"][key].is_u64(), "{done}");
        }
        id
    };
    let _daemon = daemon::start(home.path(), &[]);
    let port = api::port(home.path());
    let retry = Request::new("POST", "/v1/runs")
        .auth("default")
        .header("Idempotency-Key", "ticket-123")
        .json(&body)
        .send(port);
    assert_eq!(retry.status, 202);
    assert_eq!(retry.header("idempotency-replayed"), Some("true"));
    assert_eq!(retry.json()["run_id"], id);
    let conflict = Request::new("POST", "/v1/runs")
        .auth("default")
        .header("Idempotency-Key", "ticket-123")
        .json(&json!({"input": "other work"}))
        .send(port);
    assert_eq!(conflict.status, 409);
    assert_eq!(conflict.json()["error"]["code"], "idempotency_key_conflict");
    let unkeyed = run_id(&create(port, &body));
    assert_ne!(unkeyed, id, "no header always creates a run");
}

/// Scenario: Reconnect without loss.
#[test]
fn a_reconnecting_client_resumes_after_its_last_event() {
    let home = api::home("");
    script(
        home.path(),
        "[[rule]]\nstep = 0\n[[rule.tool_calls]]\nname = \"terminal\"\narguments = { command = \"sleep 2\" }\n\
         [[rule]]\ntext = \"slept\"\nrepeat = true\n",
    );
    let _daemon = daemon::start(home.path(), &[]);
    let port = api::port(home.path());
    let id = run_id(&create(port, &json!({"input": "nap"})));
    let seen = events(&id, None).stream_until(port, |(_, event, _)| {
        event.as_deref() == Some("tool.started")
    });
    let (last_id, ..) = seen.last().unwrap().clone();
    let last_id = last_id.unwrap();
    // The client is gone; it comes back with the last id it saw.
    let resumed = events(&id, Some(&last_id)).send(port).events();
    let first = &resumed[0];
    assert_eq!(
        first.0.as_deref().unwrap().parse::<u64>().unwrap(),
        last_id.parse::<u64>().unwrap() + 1,
        "{resumed:?}"
    );
    assert_eq!(first.1.as_deref(), Some("tool.completed"));
    let terminal: Vec<_> = resumed
        .iter()
        .filter(|(_, e, _)| {
            e.as_deref()
                .is_some_and(|e| e.starts_with("run.") && e != "run.started")
        })
        .collect();
    assert_eq!(terminal.len(), 1, "exactly one terminal event");
    assert_eq!(terminal[0].1.as_deref(), Some("run.completed"));
    let data: Value = serde_json::from_str(&terminal[0].2).unwrap();
    assert!(
        data["usage"].is_object() && data["runtime"]["provider"] == "mock",
        "{data}"
    );
}

fn approval_script(home: &Path) {
    script(
        home,
        "[[rule]]\ntool_result_regex = \"^BLOCKED: \"\ntext = \"refused\"\nrepeat = true\n\
         [[rule]]\nuser = \"clean\"\nrepeat = true\n[[rule.tool_calls]]\nname = \"terminal\"\n\
         arguments = { command = \"rm -rf build\" }\n",
    );
    std::fs::create_dir_all(home.join("profiles/default/workspace/build")).unwrap();
}

/// Scenario: Stale approval generation. Scenario: Approval refusal
/// distinguishable.
#[test]
fn approvals_are_pending_controls_answered_with_their_generation() {
    let home = api::home("");
    approval_script(home.path());
    let _daemon = daemon::start(home.path(), &[]);
    let port = api::port(home.path());
    let id = run_id(&create(port, &json!({"input": "clean the build"})));
    let waiting = run_until(port, "default", &id, |r| {
        r["status"] == "waiting_for_approval"
    });
    let pending = waiting["pending_controls"][0].clone();
    let request_id = pending["request_id"].as_str().unwrap().to_owned();
    let generation = pending["execution_generation"].as_i64().unwrap();
    let stale = control(
        port,
        &id,
        "approval",
        &json!({"request_id": request_id, "execution_generation": generation - 1, "choice": "once"}),
    );
    assert_eq!(stale.status, 409);
    assert_eq!(stale.json()["error"]["code"], "stale_generation");
    let still = Request::new("GET", &format!("/v1/runs/{id}"))
        .auth("default")
        .send(port)
        .json();
    assert_eq!(still["status"], "waiting_for_approval");
    assert_eq!(
        still["pending_controls"][0]["request_id"],
        request_id.as_str()
    );
    let extra = control(
        port,
        &id,
        "approval",
        &json!({"request_id": request_id, "execution_generation": generation, "choice": "deny", "why": "x"}),
    );
    assert_eq!(extra.status, 400);
    assert_eq!(extra.json()["error"]["code"], "invalid_params");
    let foreign = control(
        port,
        &id,
        "approval",
        &json!({"request_id": "srq-000000000000", "execution_generation": generation, "choice": "deny"}),
    );
    assert_eq!(foreign.status, 409);
    let denied = control(
        port,
        &id,
        "approval",
        &json!({"request_id": request_id, "execution_generation": generation, "choice": "deny"}),
    );
    assert_eq!(denied.status, 200, "{}", denied.body);
    let done = run_until_terminal(port, "default", &id);
    assert_eq!(done["output"], "refused");
    let all = events(&id, None).send(port).events();
    let asked = all
        .iter()
        .find(|(_, e, _)| e.as_deref() == Some("approval.request"))
        .unwrap();
    let asked: Value = serde_json::from_str(&asked.2).unwrap();
    assert_eq!(asked["request_id"], request_id.as_str());
    let completed = all
        .iter()
        .find(|(_, e, _)| e.as_deref() == Some("tool.completed"))
        .unwrap();
    let completed: Value = serde_json::from_str(&completed.2).unwrap();
    assert_eq!(completed["error"], true, "{completed}");
    assert!(
        completed["preview"]
            .as_str()
            .unwrap()
            .starts_with("BLOCKED:"),
        "{completed}"
    );
    assert!(
        home.path()
            .join("profiles/default/workspace/build")
            .is_dir()
    );
}

#[test]
fn a_stopped_run_ends_cancelled_and_steering_reaches_a_running_one() {
    let home = api::home("");
    script(
        home.path(),
        "[[rule]]\nstep = 0\n[[rule.tool_calls]]\nname = \"terminal\"\narguments = { command = \"sleep 20\" }\n\
         [[rule]]\ntext = \"finished\"\nrepeat = true\n",
    );
    let _daemon = daemon::start(home.path(), &[]);
    let port = api::port(home.path());
    let id = run_id(&create(port, &json!({"input": "long work"})));
    run_until(port, "default", &id, |r| r["status"] == "running");
    let steer = control(port, &id, "steer", &json!({"text": "hurry up"}));
    assert_eq!(steer.status, 200, "{}", steer.body);
    let stop = control(port, &id, "stop", &json!({}));
    assert_eq!(stop.json(), json!({"status": "stopping"}));
    let done = run_until_terminal(port, "default", &id);
    assert_eq!(done["status"], "cancelled", "{done}");
}

/// Scenario: Graceful stop mid-run.
#[test]
fn a_run_active_at_shutdown_is_recorded_interrupted() {
    let home = common::mock_home();
    api::enable(home.path(), "default", "");
    api::listen(home.path(), "\n[daemon]\nrestart_drain_secs = 1");
    script(
        home.path(),
        "[[rule]]\nlatency_ms = 30000\ntext = \"too late\"\nrepeat = true\n",
    );
    let daemon = daemon::start(home.path(), &[]);
    let port = api::port(home.path());
    let id = run_id(&create(port, &json!({"input": "slow work"})));
    run_until(port, "default", &id, |r| r["status"] == "running");
    drop(daemon);
    let _daemon = daemon::start(home.path(), &[]);
    let port = api::port(home.path());
    let after = Request::new("GET", &format!("/v1/runs/{id}"))
        .auth("default")
        .send(port)
        .json();
    assert_eq!(after["status"], "interrupted", "{after}");
    assert_eq!(after["error"], "Gateway shutdown interrupted the run.");
    assert_eq!(
        after["output"],
        Value::Null,
        "the late result was not recorded"
    );
}

/// Scenario: Fallback reflected in runtime.
#[test]
fn the_runtime_names_the_fallback_pair_that_served_the_run() {
    let home = api::home("");
    ok(&run(
        home.path(),
        &["config", "set", "model.default", "primary"],
    ));
    append_config(
        home.path(),
        "\n[[fallback.chain]]\nprovider = \"mock\"\nmodel = \"secondary\"\n",
    );
    script(
        home.path(),
        "[[rule]]\nmodel = \"primary\"\nfault = \"rate_limit\"\nrepeat = true\n\
         [[rule]]\nmodel = \"secondary\"\ntext = \"answered by the secondary\"\nrepeat = true\n",
    );
    let _daemon = daemon::start(home.path(), &[]);
    let port = api::port(home.path());
    let id = run_id(&create(port, &json!({"input": "hi"})));
    let done = run_until_terminal(port, "default", &id);
    assert_eq!(done["output"], "answered by the secondary", "{done}");
    assert_eq!(done["runtime"]["provider"], "mock");
    assert_eq!(done["runtime"]["model"], "secondary");
}

/// Scenario: Unknown run resolved.
#[cfg(unix)]
#[test]
fn an_unknown_run_is_resolved_and_its_session_follower_runs() {
    let home = api::home("");
    script(
        home.path(),
        "[[rule]]\nuser = \"lost\"\nlatency_ms = 60000\ntext = \"never seen\"\nrepeat = true\n\
         [[rule]]\nuser = \"follow\"\ntext = \"follower answered\"\nrepeat = true\n",
    );
    let crashed = daemon::start(home.path(), &[]);
    let port = api::port(home.path());
    let lost = create(port, &json!({"input": "lost work"}));
    let lost_id = run_id(&lost);
    let session = lost.json()["session_id"].as_str().unwrap().to_owned();
    run_until(port, "default", &lost_id, |r| r["status"] == "running");
    let pid = crashed.pid;
    std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .unwrap();
    std::mem::forget(crashed);
    for _ in 0..100 {
        if !std::path::Path::new(&format!("/proc/{pid}")).exists()
            && std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .status()
                .is_ok_and(|s| !s.success())
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let _daemon = daemon::start(home.path(), &[]);
    let port = api::port(home.path());
    let unknown = Request::new("GET", &format!("/v1/runs/{lost_id}"))
        .auth("default")
        .send(port)
        .json();
    assert_eq!(unknown["status"], "unknown", "{unknown}");
    let stop = control(port, &lost_id, "stop", &json!({}));
    assert_eq!(stop.status, 409);
    assert_eq!(stop.json()["error"]["code"], "unknown_execution");
    let follower = run_id(&create(
        port,
        &json!({"input": "follow up", "session_id": session}),
    ));
    std::thread::sleep(std::time::Duration::from_millis(500));
    let waiting = Request::new("GET", &format!("/v1/runs/{follower}"))
        .auth("default")
        .send(port)
        .json();
    assert_eq!(
        waiting["status"], "queued",
        "the follower waits behind the unknown run"
    );
    let identity = json!({"admission_id": unknown["admission_id"], "execution_generation": unknown["execution_generation"]});
    let stale = control(
        port,
        &lost_id,
        "resolve-unknown",
        &json!({"admission_id": unknown["admission_id"], "execution_generation": 99}),
    );
    assert_eq!(stale.json()["error"]["code"], "stale_generation");
    let receipt = control(port, &lost_id, "resolve-unknown", &identity);
    assert_eq!(receipt.status, 200, "{}", receipt.body);
    assert_eq!(receipt.json()["status"], "interrupted");
    assert_eq!(
        control(port, &lost_id, "resolve-unknown", &identity).json(),
        receipt.json()
    );
    let done = run_until_terminal(port, "default", &follower);
    assert_eq!(done["output"], "follower answered", "{done}");
    let replies: Vec<String> = common::session_messages(home.path(), &session)
        .iter()
        .filter(|m| m["role"] == "assistant")
        .filter_map(|m| m["content"].as_str().map(str::to_owned))
        .collect();
    assert!(
        !replies.iter().any(|r| r == "never seen"),
        "the lost turn was not replayed: {replies:?}"
    );
}

/// Run events stream: a `tool.completed` preview passes through forced
/// secret redaction, even with `security.redact_secrets = false`.
#[test]
fn tool_previews_redact_known_secrets_even_when_redaction_is_off() {
    const VALUE: &str = "horse-battery-staple-9417";
    let home = api::home("");
    ok(&run(
        home.path(),
        &["config", "set", "security.redact_secrets", "false"],
    ));
    ok(&common::run_with_stdin(
        home.path(),
        &["secrets", "set", "TICKET_TOKEN"],
        &format!("{VALUE}\n"),
    ));
    std::fs::create_dir_all(home.path().join("profiles/default/workspace")).unwrap();
    std::fs::write(
        home.path().join("profiles/default/workspace/notes.txt"),
        format!("token {VALUE}\n"),
    )
    .unwrap();
    script(
        home.path(),
        "[[rule]]\nstep = 0\n[[rule.tool_calls]]\nname = \"read_file\"\narguments = { path = \"notes.txt\" }\n\
         [[rule]]\ntext = \"read\"\nrepeat = true\n",
    );
    let _daemon = daemon::start(home.path(), &[]);
    let port = api::port(home.path());
    let id = run_id(&create(port, &json!({"input": "read the notes"})));
    run_until_terminal(port, "default", &id);
    let all = events(&id, None).send(port).events();
    let completed = all
        .iter()
        .find(|(_, e, _)| e.as_deref() == Some("tool.completed"))
        .unwrap();
    let completed: Value = serde_json::from_str(&completed.2).unwrap();
    let preview = completed["preview"].as_str().unwrap();
    assert!(preview.contains("[REDACTED:TICKET_TOKEN]"), "{completed}");
    assert!(!preview.contains(VALUE), "{completed}");
}

//! REST over sessions and scheduled jobs, and the discovery and readiness
//! endpoints of the daemon's API server (spec: api-server, Sessions REST
//! API; Jobs REST API; Discovery and readiness endpoints).

#![cfg(feature = "gateway")]

mod common;

use std::path::Path;

use base64::Engine as _;
use serde_json::{Value, json};

use self::common::api::{self, Request, chat};
use self::common::{PNG, append_config, daemon, ok, run};

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

fn get(port: u16, path: &str) -> api::Answer {
    Request::new("GET", path).auth("default").send(port)
}

fn post(port: u16, path: &str, body: &Value) -> api::Answer {
    Request::new("POST", path)
        .auth("default")
        .json(body)
        .send(port)
}

/// Scenario: Fork and continue.
#[test]
fn a_fork_continues_with_its_parents_history() {
    let (home, port) = api::home("");
    script(
        home.path(),
        "[[rule]]\nstep = 1\nuser = \"what was it\"\ntext = \"saw the parent turn\"\nrepeat = true\n\
         [[rule]]\ntext = \"noted\"\nrepeat = true\n",
    );
    let _daemon = daemon::start(home.path(), &[]);
    let first = chat(
        port,
        "default",
        &json!({"messages": [{"role": "user", "content": "the code word is otter"}]}),
    );
    let parent = first.header("x-prometheus-session-id").unwrap().to_owned();
    let forked = post(port, &format!("/api/sessions/{parent}/fork"), &json!({}));
    assert_eq!(forked.status, 201, "{}", forked.body);
    let forked = forked.json();
    assert_eq!(forked["parent_session_id"], parent.as_str());
    assert_eq!(forked["lineage_kind"], "fork");
    let child = forked["id"].as_str().unwrap();
    let reply = post(
        port,
        &format!("/api/sessions/{child}/chat"),
        &json!({"message": "what was it"}),
    );
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(reply.json()["status"], "completed");
    assert_eq!(reply.json()["reply"], "saw the parent turn");
    let streamed = post(
        port,
        &format!("/api/sessions/{child}/chat/stream"),
        &json!({"message": "again"}),
    );
    let events = streamed.events();
    assert!(
        events
            .iter()
            .any(|(_, e, _)| e.as_deref() == Some("assistant.delta")),
        "{events:?}"
    );
    assert_eq!(events.last().unwrap().1.as_deref(), Some("run.completed"));
    let listed = get(port, "/api/sessions?limit=200&include_children=true").json();
    let ids: Vec<&str> = listed["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["id"].as_str())
        .collect();
    assert!(
        ids.contains(&child) && ids.contains(&parent.as_str()),
        "{listed}"
    );
    let roots = get(port, "/api/sessions").json();
    assert!(
        !roots["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["id"] == child),
        "children hidden by default"
    );
    let renamed = Request::new("PATCH", &format!("/api/sessions/{child}"))
        .auth("default")
        .json(&json!({"title": "Otter talk"}))
        .send(port);
    assert_eq!(renamed.json()["title"], "Otter talk");
    let deleted = Request::new("DELETE", &format!("/api/sessions/{child}"))
        .auth("default")
        .send(port);
    assert_eq!(deleted.status, 200);
    assert_eq!(get(port, &format!("/api/sessions/{child}")).status, 404);
}

#[test]
fn sessions_of_other_surfaces_are_readable_but_not_writable() {
    let (home, port) = api::home("");
    let _daemon = daemon::start(home.path(), &[]);
    let out = ok(&run(home.path(), &["--json", "-z", "a cli turn"]));
    let report: Value = serde_json::from_str(&out).unwrap();
    let foreign = report["session_id"].as_str().unwrap();
    let shown = get(port, &format!("/api/sessions/{foreign}"));
    assert_eq!(shown.json()["source"], "cli");
    assert_eq!(
        get(port, &format!("/api/sessions/{foreign}/messages")).status,
        200
    );
    for (method, path, body) in [
        (
            "POST",
            format!("/api/sessions/{foreign}/chat"),
            json!({"message": "hi"}),
        ),
        (
            "PATCH",
            format!("/api/sessions/{foreign}"),
            json!({"title": "x"}),
        ),
        ("DELETE", format!("/api/sessions/{foreign}"), json!({})),
    ] {
        let refused = Request::new(method, &path)
            .auth("default")
            .json(&body)
            .send(port);
        assert_eq!(refused.status, 409, "{method} {path}");
        assert_eq!(refused.json()["error"]["code"], "permission_denied");
    }
    let created = post(port, "/api/sessions", &json!({"title": "Empty"}));
    assert_eq!(created.status, 201);
    assert_eq!(created.json()["source"], "api");
}

/// Scenario: Light transcript.
#[test]
fn a_light_transcript_shows_image_placeholders() {
    let (home, port) = api::home("");
    let _daemon = daemon::start(home.path(), &[]);
    let png = base64::engine::general_purpose::STANDARD.encode(PNG);
    let answer = chat(
        port,
        "default",
        &json!({"messages": [{"role": "user", "content": [
            {"type": "text", "text": "what is this"},
            {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{png}")}},
        ]}]}),
    );
    assert_eq!(answer.status, 200, "{}", answer.body);
    let session = answer.header("x-prometheus-session-id").unwrap();
    let light = get(
        port,
        &format!("/api/sessions/{session}/messages?inline_images=false"),
    )
    .json();
    let user = light["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "user")
        .unwrap()
        .clone();
    assert!(
        user["content"].as_str().unwrap().contains("[image]"),
        "{user}"
    );
    assert!(user.get("images").is_none());
    let full = get(port, &format!("/api/sessions/{session}/messages")).json();
    let user = full["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "user")
        .unwrap()
        .clone();
    let url = user["images"][0]["image_url"]["url"].as_str().unwrap();
    assert!(url.starts_with("data:image/png;base64,"), "{user}");
}

/// Scenario: Past one-shot resume.
#[test]
fn jobs_follow_the_cron_rules_over_rest() {
    let (home, port) = api::home("");
    let _daemon = daemon::start(home.path(), &[]);
    let soon = (jiff::Timestamp::now() + jiff::SignedDuration::from_secs(2)).to_string();
    let job = json!({"schedule": soon, "prompt": "say hi", "paused": true, "name": "soon"});
    let asked = post(port, "/api/jobs", &job);
    assert_eq!(asked.status, 200, "{}", asked.body);
    assert_eq!(asked.json()["status"], "needs_confirmation");
    let mut confirmed = job.clone();
    confirmed["confirm_token"] = asked.json()["confirm_token"].clone();
    let created = post(port, "/api/jobs", &confirmed);
    assert_eq!(created.status, 201, "{}", created.body);
    let id = created.json()["job"]["id"].as_str().unwrap().to_owned();
    let db = rusqlite::Connection::open(home.path().join("profiles/default/state.db")).unwrap();
    let origin: String = db
        .query_row("SELECT origin FROM cron_jobs WHERE id = ?1", [&id], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&origin).unwrap()["kind"],
        "api",
        "{origin}"
    );
    std::thread::sleep(std::time::Duration::from_secs(3));
    let resumed = post(port, &format!("/api/jobs/{id}/resume"), &json!({}));
    assert_eq!(resumed.status, 400, "{}", resumed.body);
    let message = resumed.json()["error"]["message"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        message.starts_with("cannot resume: one-shot time ")
            && message.ends_with("is in the past; use 'prometheus cron edit --schedule'"),
        "{message}"
    );
    assert_eq!(get(port, "/api/jobs/nosuchjob").status, 404);
    let invalid = post(
        port,
        "/api/jobs",
        &json!({"schedule": "every 5s", "prompt": "x"}),
    );
    assert_eq!(invalid.status, 400);
    assert_eq!(
        get(port, "/api/jobs").json()["data"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let removed = Request::new("DELETE", &format!("/api/jobs/{id}"))
        .auth("default")
        .send(port);
    assert_eq!(removed.status, 200);
}

#[test]
fn discovery_endpoints_describe_the_profile() {
    let (home, port) = api::home("");
    let _daemon = daemon::start(home.path(), &[]);
    let capabilities = get(port, "/v1/capabilities").json();
    assert_eq!(capabilities["object"], "prometheus.api_server.capabilities");
    assert_eq!(
        capabilities["auth"],
        json!({"type": "bearer", "required": true})
    );
    for feature in [
        "chat_completions",
        "responses_api",
        "run_submission",
        "run_events_sse",
        "run_unknown_resolution",
        "sessions_api",
        "jobs_api",
        "session_key_header",
    ] {
        assert_eq!(capabilities["features"][feature], true, "{feature}");
    }
    let skills = get(port, "/v1/skills").json();
    let first = &skills["data"][0];
    assert!(
        first["name"].is_string()
            && first["description"].is_string()
            && first["category"].is_string(),
        "{skills}"
    );
    let toolsets = get(port, "/v1/toolsets").json();
    let terminal = toolsets["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "terminal")
        .unwrap()
        .clone();
    assert_eq!(terminal["enabled"], true, "{terminal}");
    assert!(
        terminal["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t == "terminal")
    );
    let options = get(port, "/api/model/options");
    assert_eq!(options.status, 200, "{}", options.body);
    assert!(options.body.contains("mock"), "{}", options.body);
}

/// Scenario: Readiness hides secrets.
#[test]
fn detailed_health_reports_failures_without_secrets_or_paths() {
    let (home, port) = api::home("");
    ok(&run(
        home.path(),
        &["config", "set", "model.provider", "openai"],
    ));
    ok(&run(
        home.path(),
        &["config", "set", "model.default", "gpt-4o"],
    ));
    let _daemon = daemon::start(home.path(), &[]);
    let unauthenticated = Request::new("GET", "/health/detailed").send(port);
    assert_eq!(unauthenticated.status, 401);
    let health = get(port, "/health/detailed");
    assert_eq!(health.status, 200);
    let body = health.json();
    assert_eq!(body["status"], "degraded", "{body}");
    assert_eq!(body["checks"]["model"]["status"], "failed");
    let code = body["checks"]["model"]["code"].as_str().unwrap();
    assert!(
        code.contains('.') && !code.contains(' '),
        "a stable code: {code}"
    );
    assert_eq!(body["checks"]["state_db"]["status"], "ok");
    let text = health.body.clone();
    assert!(
        !text.contains(&home.path().to_string_lossy().into_owned()),
        "no paths: {text}"
    );
    assert!(
        !text.contains("test-key-default"),
        "no key material: {text}"
    );
    assert!(
        !text.contains("OPENAI_API_KEY"),
        "no credential names or messages: {text}"
    );
}

//! The OpenAI-compatible API server of a running daemon, over HTTP (spec:
//! api-server): the opt-in listener that fails closed, profile-bound
//! bearer authentication, browser and Host checks, model listing, Chat
//! Completions with session identity, caller instructions, unattended
//! approvals and per-request model selection, and the run cap.

#![cfg(feature = "gateway")]

mod common;

use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};

use self::common::api::{self, Request, chat};
use self::common::gateway::{Daemon, gateway_home, send, texts, wait};
use self::common::{append_config, daemon, mock_home, run, stderr};

/// Points the default profile's mock provider at a script with `rules`.
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

fn user(text: &str) -> Value {
    json!({"messages": [{"role": "user", "content": text}]})
}

fn sessions_of(home: &Path) -> usize {
    let listed = common::ok(&run(home, &["--json", "sessions", "list", "--all"]));
    let listed: Value = serde_json::from_str(&listed).unwrap();
    listed["sessions"].as_array().unwrap().len()
}

/// Scenario: Enabled without a key.
#[test]
fn a_profile_without_a_key_is_not_served_but_health_answers() {
    let home = mock_home();
    append_config(home.path(), "\n[api_server]\nenabled = true\n");
    api::listen(home.path(), "");
    let _daemon = daemon::start(home.path(), &[]);
    let port = api::port(home.path());
    let health = Request::new("GET", "/health").send(port);
    assert_eq!(health.status, 200);
    assert_eq!(health.json(), json!({"status": "ok"}));
    assert_eq!(
        Request::new("GET", "/v1/health").send(port).json(),
        json!({"status": "ok"})
    );
    let models = Request::new("GET", "/v1/models").auth("default").send(port);
    assert_eq!(models.status, 404, "{}", models.body);
    let log = std::fs::read_to_string(home.path().join("logs/gateway.log")).unwrap_or_default();
    assert!(
        log.contains("api_server disabled for default: missing or weak key"),
        "{log}"
    );
}

/// Scenario: Public bind needs explicit opt-in.
#[test]
fn a_public_bind_without_opt_in_fails_validation() {
    let home = mock_home();
    std::fs::write(
        home.path().join("daemon.toml"),
        "[api_server]\nlisten = \"0.0.0.0:8642\"\n",
    )
    .unwrap();
    let out = run(home.path(), &["daemon", "run"]);
    assert_eq!(out.status.code(), Some(78), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("non-loopback api_server.listen requires api_server.public = true"),
        "{}",
        stderr(&out)
    );
}

/// Scenario: Default key on a named prefix. Scenario: Two users, two
/// models. Scenario: Run id from another profile.
#[test]
fn each_profile_answers_only_its_own_key_and_model_and_ids() {
    let home = mock_home();
    api::enable(home.path(), "default", "");
    api::multiplex(home.path());
    api::profile(home.path(), "alice", "");
    api::profile(home.path(), "bob", "");
    api::listen(home.path(), "");
    let _daemon = daemon::start(home.path(), &[]);
    let port = api::port(home.path());
    let wrong = Request::new("GET", "/p/alice/v1/models")
        .auth("default")
        .send(port);
    assert_eq!(wrong.status, 401);
    assert_eq!(wrong.json()["error"]["type"], "authentication_error");
    assert_eq!(wrong.json()["error"]["code"], "invalid_api_key");
    for (prefix, profile, model) in [
        ("/p/alice", "alice", "alice"),
        ("/p/bob", "bob", "bob"),
        ("", "default", "prometheus-agent"),
    ] {
        let models = Request::new("GET", &format!("{prefix}/v1/models"))
            .auth(profile)
            .send(port);
        assert_eq!(models.status, 200, "{}", models.body);
        let data = models.json()["data"].clone();
        assert_eq!(data[0]["id"], model);
        assert_eq!(data[0]["owned_by"], "prometheus");
        assert_eq!(data.as_array().unwrap().len(), 1);
    }
    let unserved = Request::new("GET", "/p/carol/v1/models")
        .auth("default")
        .send(port);
    assert_eq!(unserved.status, 404);
    let created = Request::new("POST", "/p/alice/v1/runs")
        .auth("alice")
        .json(&json!({"input": "hello"}))
        .send(port);
    assert_eq!(created.status, 202, "{}", created.body);
    let id = created.json()["run_id"].as_str().unwrap().to_owned();
    assert!(id.starts_with("run_"));
    let foreign = Request::new("GET", &format!("/p/bob/v1/runs/{id}"))
        .auth("bob")
        .send(port);
    assert_eq!(foreign.status, 404);
    let own = Request::new("GET", &format!("/p/alice/v1/runs/{id}"))
        .auth("alice")
        .send(port);
    assert_eq!(own.status, 200);
    // The alice session lives in alice's store only.
    let session = own.json()["session_id"].as_str().unwrap().to_owned();
    let listed = common::ok(&run(
        home.path(),
        &["-p", "bob", "--json", "sessions", "list", "--all"],
    ));
    assert!(!listed.contains(&session));
}

/// Scenario: Web page on another origin. Scenario: Rebinding host refused.
#[test]
fn browser_origins_and_foreign_hosts_are_refused_before_any_work() {
    let home = api::home("");
    let _daemon = daemon::start(home.path(), &[]);
    let port = api::port(home.path());
    let evil = Request::new("POST", "/v1/chat/completions")
        .auth("default")
        .header("Origin", "https://evil.example")
        .json(&user("steal"))
        .send(port);
    assert_eq!(evil.status, 403);
    assert_eq!(evil.header("x-content-type-options"), Some("nosniff"));
    assert_eq!(evil.header("access-control-allow-origin"), None);
    let rebound = Request::new("GET", "/v1/models")
        .auth("default")
        .header("Host", &format!("attacker.example:{port}"))
        .send(port);
    assert_eq!(rebound.status, 403);
    assert_eq!(sessions_of(home.path()), 0, "no run started");
    let fine = Request::new("GET", "/v1/models").auth("default").send(port);
    assert_eq!(fine.header("referrer-policy"), Some("no-referrer"));
}

#[test]
fn an_allowlisted_origin_gets_cors_headers_and_preflights() {
    let home = mock_home();
    api::enable(home.path(), "default", "");
    api::listen(home.path(), "cors_origins = [\"http://localhost:3000\"]");
    let _daemon = daemon::start(home.path(), &[]);
    let port = api::port(home.path());
    let preflight = Request::new("OPTIONS", "/v1/chat/completions")
        .header("Origin", "http://localhost:3000")
        .header("Access-Control-Request-Method", "POST")
        .send(port);
    assert_eq!(preflight.status, 204);
    assert_eq!(preflight.header("access-control-max-age"), Some("600"));
    assert!(
        preflight
            .header("access-control-allow-headers")
            .unwrap()
            .contains("X-Prometheus-Session-Key")
    );
    let streamed = chat(
        port,
        "default",
        &json!({"stream": true, "messages": [{"role": "user", "content": "hi"}]}),
    );
    assert_eq!(streamed.status, 200);
    let allowed = Request::new("POST", "/v1/chat/completions")
        .auth("default")
        .header("Origin", "http://localhost:3000")
        .json(&json!({"stream": true, "messages": [{"role": "user", "content": "hi"}]}))
        .send(port);
    assert_eq!(
        allowed.header("access-control-allow-origin"),
        Some("http://localhost:3000")
    );
}

/// Scenario: Unknown OpenAI field ignored.
#[test]
fn unknown_openai_fields_are_ignored() {
    let home = api::home("");
    let _daemon = daemon::start(home.path(), &[]);
    let port = api::port(home.path());
    let answer = chat(
        port,
        "default",
        &json!({"model": "prometheus-agent", "logit_bias": {}, "temperature": 0.2,
            "messages": [{"role": "user", "content": "hello"}]}),
    );
    assert_eq!(answer.status, 200, "{}", answer.body);
    let body = answer.json();
    assert_eq!(body["object"], "chat.completion");
    assert_eq!(body["choices"][0]["message"]["content"], "mock: hello");
    assert_eq!(body["choices"][0]["finish_reason"], "stop");
    for key in [
        "prompt_tokens",
        "completion_tokens",
        "cache_read_tokens",
        "cache_write_tokens",
    ] {
        assert!(body["usage"][key].is_u64(), "{key}: {}", body["usage"]);
    }
    let session = answer.header("x-prometheus-session-id").unwrap();
    assert_eq!(body["prometheus"]["session_id"], session);
    let listed = common::ok(&run(home.path(), &["--json", "sessions", "list", "--all"]));
    assert!(listed.contains("\"api\""), "{listed}");
}

/// Scenario: Budget exhausted is not success.
#[test]
fn an_exhausted_iteration_budget_answers_length() {
    let home = api::home("");
    append_config(home.path(), "\n[agent]\nmax_iterations = 1\n");
    script(
        home.path(),
        "[[rule]]\nuser = \"reached this turn's budget\"\ntext = \"wrap-up\"\nrepeat = true\n\
         [[rule]]\nrepeat = true\n[[rule.tool_calls]]\nname = \"todo\"\narguments = { todos = [] }\n",
    );
    let _daemon = daemon::start(home.path(), &[]);
    let port = api::port(home.path());
    let answer = chat(port, "default", &user("loop forever"));
    let body = answer.json();
    assert_eq!(body["choices"][0]["finish_reason"], "length", "{body}");
    assert_eq!(
        body["prometheus"]["error_code"],
        "iteration_budget_exhausted"
    );
    assert_eq!(body["choices"][0]["message"]["content"], "wrap-up");
}

/// Scenario: Uploaded file refused.
#[test]
fn an_uploaded_file_is_refused_before_a_turn() {
    let home = api::home("");
    let _daemon = daemon::start(home.path(), &[]);
    let port = api::port(home.path());
    let answer = chat(
        port,
        "default",
        &json!({"messages": [{"role": "user", "content": [
            {"type": "text", "text": "summarize"},
            {"type": "file", "file": {"file_id": "file-1"}},
        ]}]}),
    );
    assert_eq!(answer.status, 400);
    assert_eq!(answer.json()["error"]["code"], "unsupported_content_type");
    assert_eq!(sessions_of(home.path()), 0);
}

/// Scenario: Header continues the session.
#[test]
fn the_session_header_continues_from_stored_history() {
    let home = api::home("");
    script(
        home.path(),
        "[[rule]]\nstep = 2\ntext = \"saw two prior turns\"\nrepeat = true\n\
         [[rule]]\ntext = \"ok\"\nrepeat = true\n",
    );
    let _daemon = daemon::start(home.path(), &[]);
    let port = api::port(home.path());
    let first = chat(port, "default", &user("first"));
    let session = first.header("x-prometheus-session-id").unwrap().to_owned();
    let with = |text: &str| {
        Request::new("POST", "/v1/chat/completions")
            .auth("default")
            .header("X-Hermes-Session-Id", &session)
            .json(&user(text))
            .send(port)
    };
    assert_eq!(
        with("second").json()["choices"][0]["message"]["content"],
        "ok"
    );
    let third = with("third");
    assert_eq!(
        third.json()["choices"][0]["message"]["content"],
        "saw two prior turns"
    );
    assert_eq!(
        third.header("x-prometheus-session-id"),
        Some(session.as_str())
    );
    // Without the header the same opening derives the same session.
    let again = chat(
        port,
        "default",
        &json!({"messages": [{"role": "user", "content": "first"}, {"role": "assistant", "content": "ok"},
            {"role": "user", "content": "fourth"}]}),
    );
    assert_eq!(
        again.header("x-prometheus-session-id"),
        Some(session.as_str())
    );
}

/// Scenario: Foreign session refused.
#[test]
fn a_session_of_another_surface_is_refused() {
    let home = api::home("");
    let _daemon = daemon::start(home.path(), &[]);
    let port = api::port(home.path());
    let out = common::ok(&run(home.path(), &["--json", "-z", "a cli turn"]));
    let report: Value = serde_json::from_str(&out).unwrap();
    let foreign = report["session_id"].as_str().unwrap();
    let before = sessions_of(home.path());
    let refused = Request::new("POST", "/v1/chat/completions")
        .auth("default")
        .header("X-Prometheus-Session-Id", foreign)
        .header("Idempotency-Key", "k-foreign")
        .json(&user("take over"))
        .send(port);
    assert_eq!(refused.status, 409);
    assert_eq!(refused.json()["error"]["code"], "permission_denied");
    assert_eq!(sessions_of(home.path()), before);
    // No reservation was made: the key is still free for other work.
    let fresh = Request::new("POST", "/v1/chat/completions")
        .auth("default")
        .header("Idempotency-Key", "k-foreign")
        .json(&user("new work"))
        .send(port);
    assert_eq!(fresh.status, 200, "{}", fresh.body);
}

fn stored_prompt(home: &Path, session: &str) -> (String, String) {
    let db = rusqlite::Connection::open(home.join("profiles/default/state.db")).unwrap();
    db.query_row(
        "SELECT tiers, tools FROM session_prompts WHERE session_id = ?1",
        [session],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .unwrap()
}

/// Scenario: Instructions do not replace the agent.
#[test]
fn caller_instructions_are_layered_after_the_profile_prompt() {
    let home = api::home("");
    script(
        home.path(),
        "[[rule]]\ntools = \"terminal\"\ntext = \"Voici les fichiers\"\nrepeat = true\n\
         [[rule]]\ntext = \"no tools\"\nrepeat = true\n",
    );
    let _daemon = daemon::start(home.path(), &[]);
    let port = api::port(home.path());
    let answer = chat(
        port,
        "default",
        &json!({"messages": [{"role": "system", "content": "Always answer in French"},
            {"role": "user", "content": "list files"}]}),
    );
    assert_eq!(
        answer.json()["choices"][0]["message"]["content"],
        "Voici les fichiers"
    );
    let session = answer.header("x-prometheus-session-id").unwrap().to_owned();
    let (tiers, tools) = stored_prompt(home.path(), &session);
    assert!(tiers.contains("Always answer in French"), "{tiers}");
    assert!(
        tiers.contains("Prometheus"),
        "the profile's identity stays: {tiers}"
    );
    assert!(
        tools.contains("terminal") && tools.contains("read_file"),
        "{tools}"
    );
    assert!(
        !tools.contains("\"clarify\""),
        "API sessions have no clarify: {tools}"
    );
    // A continuation with other instructions gets a notice, and the
    // system prompt stays as it was.
    let changed = Request::new("POST", "/v1/chat/completions")
        .auth("default")
        .header("X-Prometheus-Session-Id", &session)
        .json(
            &json!({"messages": [{"role": "system", "content": "Answer in German"},
            {"role": "user", "content": "again"}]}),
        )
        .send(port);
    assert_eq!(changed.status, 200);
    assert_eq!(stored_prompt(home.path(), &session).0, tiers);
    let messages = common::session_messages(home.path(), &session);
    let notice = messages
        .iter()
        .filter(|m| m["role"] == "user")
        .filter_map(|m| m["content"].as_str())
        .any(|c| c.contains("Answer in German"));
    assert!(notice, "{messages:?}");
}

/// Scenario: Approval over Chat Completions.
#[test]
fn an_approval_without_an_answer_channel_is_denied_at_once() {
    let home = api::home("");
    script(
        home.path(),
        "[[rule]]\ntool_result_regex = \"^BLOCKED: \"\ntext = \"it was blocked\"\nrepeat = true\n\
         [[rule]]\nuser = \"clean\"\nrepeat = true\n[[rule.tool_calls]]\nname = \"terminal\"\n\
         arguments = { command = \"rm -rf build\" }\n",
    );
    std::fs::create_dir_all(home.path().join("profiles/default/workspace/build")).unwrap();
    let _daemon = daemon::start(home.path(), &[]);
    let port = api::port(home.path());
    let started = std::time::Instant::now();
    let answer = chat(port, "default", &user("clean the build"));
    assert_eq!(
        answer.json()["choices"][0]["message"]["content"],
        "it was blocked",
        "{}",
        answer.body
    );
    assert!(started.elapsed() < std::time::Duration::from_mins(1));
    assert!(
        home.path()
            .join("profiles/default/workspace/build")
            .is_dir()
    );
}

/// Scenario: Generic client model ignored. Scenario: Route alias conflict.
#[test]
fn request_models_follow_the_selection_order() {
    let home =
        api::home("[api_server.model_routes.fast]\nprovider = \"mock\"\nmodel = \"quick\"\n");
    script(
        home.path(),
        "[[rule]]\nmodel = \"quick\"\ntext = \"quick model\"\nrepeat = true\n\
         [[rule]]\nmodel = \"direct\"\ntext = \"direct model\"\nrepeat = true\n\
         [[rule]]\ntext = \"default model\"\nrepeat = true\n",
    );
    let _daemon = daemon::start(home.path(), &[]);
    let port = api::port(home.path());
    let generic = chat(
        port,
        "default",
        &json!({"model": "gpt-4o", "messages": [{"role": "user", "content": "a"}]}),
    );
    assert_eq!(
        generic.json()["choices"][0]["message"]["content"],
        "default model"
    );
    let runtime = &generic.json()["prometheus"]["runtime"];
    assert_eq!(runtime["route_source"], "global", "{runtime}");
    assert_eq!(runtime["model"], "mock");
    assert_eq!(runtime["requested"]["model"], "gpt-4o");
    let generic = Request::new("POST", "/v1/responses")
        .auth("default")
        .json(&json!({"model": "gpt-4o", "input": "a"}))
        .send(port);
    assert_eq!(generic.status, 200, "{}", generic.body);
    let runtime = &generic.json()["prometheus"]["runtime"];
    assert_eq!(runtime["route_source"], "global", "{runtime}");
    assert_eq!(runtime["model"], "mock");
    // A request's model changes the model call only: the session's
    // system prompt and tool list stay byte-identical.
    let session = generic
        .header("x-prometheus-session-id")
        .unwrap()
        .to_owned();
    let before = stored_prompt(home.path(), &session);
    let routed = Request::new("POST", "/v1/chat/completions")
        .auth("default")
        .header("X-Prometheus-Session-Id", &session)
        .json(&json!({"model": "fast", "messages": [{"role": "user", "content": "b"}]}))
        .send(port);
    assert_eq!(
        routed.json()["choices"][0]["message"]["content"],
        "quick model"
    );
    assert_eq!(routed.json()["prometheus"]["runtime"]["model"], "quick");
    assert_eq!(stored_prompt(home.path(), &session), before);
    let db = rusqlite::Connection::open(home.path().join("profiles/default/state.db")).unwrap();
    let built_for: String = db
        .query_row(
            "SELECT model FROM session_prompts WHERE session_id = ?1",
            [&session],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        built_for, "mock",
        "the prompt was not rebuilt for the request's model"
    );
    let direct = chat(
        port,
        "default",
        &json!({"model": "direct", "provider": "mock", "messages": [{"role": "user", "content": "c"}]}),
    );
    assert_eq!(
        direct.json()["choices"][0]["message"]["content"],
        "direct model"
    );
    assert_eq!(direct.json()["prometheus"]["runtime"]["model"], "direct");
    let before = sessions_of(home.path());
    let conflict = chat(
        port,
        "default",
        &json!({"model": "fast", "provider": "openai", "messages": [{"role": "user", "content": "d"}]}),
    );
    assert_eq!(conflict.status, 400);
    assert_eq!(conflict.json()["error"]["code"], "route_conflict");
    let unknown = chat(
        port,
        "default",
        &json!({"model": "x", "provider": "nosuch", "messages": [{"role": "user", "content": "e"}]}),
    );
    assert_eq!(unknown.status, 400);
    assert!(
        unknown.json()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("nosuch")
    );
    assert_eq!(sessions_of(home.path()), before, "nothing ran");
    // A generic run reports the global route.
    let run = Request::new("POST", "/v1/runs")
        .auth("default")
        .json(&json!({"input": "g"}))
        .send(port);
    let id = run.json()["run_id"].as_str().unwrap().to_owned();
    let done = api::run_until_terminal(port, "default", &id);
    assert_eq!(done["runtime"]["route_source"], "global");
}

/// Scenario: Port collision isolated (distribution-and-update): another
/// process holds the API port while `work` and `home` enable the API
/// server; only the listener is down, every profile's adapter runs.
#[test]
fn a_taken_api_port_marks_only_the_listener_down() {
    let home = gateway_home("");
    let home = home.path();
    api::enable(home, "default", "");
    api::multiplex(home);
    for name in ["work", "home"] {
        api::profile(home, name, "");
        append_config_of(
            home,
            name,
            "\n[gateway.platforms.test]\nenabled = true\nallow_all = true\n",
        );
    }
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().port();
    std::fs::write(
        home.join("daemon.toml"),
        format!("[api_server]\nlisten = \"127.0.0.1:{port}\"\n"),
    )
    .unwrap();
    let daemon = Daemon::start(home);
    let reason = format!("port {port} already in use");
    let status = || -> Value {
        let out = run(home, &["--json", "daemon", "status"]);
        serde_json::from_slice::<Value>(&out.stdout).unwrap_or_default()["daemon"].clone()
    };
    wait(
        "every profile's adapter runs",
        Duration::from_secs(30),
        || {
            let adapters = status()["adapters"].as_array().cloned().unwrap_or_default();
            ["default", "work", "home"].iter().all(|profile| {
                adapters.iter().any(|a| {
                    // The file adapter stays `degraded` (unconfirmed) until it
                    // has carried a message; both states mean it runs.
                    a["profile"] == *profile
                        && a["platform"] == "test"
                        && matches!(a["state"].as_str(), Some("connected" | "degraded"))
                })
            })
        },
    );
    let health = status()["health"].clone();
    let listener = health
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["component"] == "listener:api_server")
        .cloned()
        .unwrap_or_else(|| panic!("no API listener component: {health}"));
    assert_eq!(listener["state"], "down", "{listener}");
    assert_eq!(listener["reason"], reason.as_str(), "{listener}");
    send(home, "work", "u1", "hello");
    wait(
        "work answers on its adapter",
        Duration::from_mins(1),
        || !texts(home, "work").is_empty(),
    );
    drop(taken);
    assert_eq!(daemon.stop(home), Some(0));
}

fn append_config_of(home: &Path, profile: &str, text: &str) {
    let config = home.join("profiles").join(profile).join("config.toml");
    let mut current = std::fs::read_to_string(&config).unwrap();
    current.push_str(text);
    std::fs::write(&config, current).unwrap();
}

/// Scenario: Eleventh run refused (with a cap of one).
#[test]
fn runs_past_the_concurrent_cap_are_refused() {
    let home = api::home("max_concurrent_runs = 1");
    script(
        home.path(),
        "latency_ms = 3000\n[[rule]]\ntext = \"slow\"\nrepeat = true\n",
    );
    let _daemon = daemon::start(home.path(), &[]);
    let port = api::port(home.path());
    let first = Request::new("POST", "/v1/runs")
        .auth("default")
        .json(&json!({"input": "one"}))
        .send(port);
    assert_eq!(first.status, 202);
    let second = Request::new("POST", "/v1/runs")
        .auth("default")
        .json(&json!({"input": "two"}))
        .send(port);
    assert_eq!(second.status, 429);
    assert_eq!(
        second.json()["error"]["message"],
        "Too many concurrent runs (max 1)"
    );
    assert!(second.header("retry-after").is_some());
    let id = first.json()["run_id"].as_str().unwrap().to_owned();
    api::run_until_terminal(port, "default", &id);
    let third = Request::new("POST", "/v1/runs")
        .auth("default")
        .json(&json!({"input": "three"}))
        .send(port);
    assert_eq!(third.status, 202);
}

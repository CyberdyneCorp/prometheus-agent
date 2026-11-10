//! Subscription quota in `auth status`, the ledger by billing source and
//! `prometheus providers health` (spec: model-providers, Subscription plan
//! and quota visibility; Billing source transparency; Provider health
//! scoring).

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jiff::Timestamp;
use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
use pmt_obs::usage::{Call, ConfigPrices, UsageRecorder};
use pmt_store::provider_state::CallSample;
use pmt_store::{NewSession, Source, Store, Tokens};
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use common::{ok, run, temp_home};

fn store(home: &Path) -> Arc<Store> {
    let home = Home::open(home).unwrap();
    let dir = home.profile(&ProfileName::default_profile()).unwrap();
    let scope = ProfileScope::open(dir, &Overrides::default(), Launch::Other).unwrap();
    Arc::new(Store::open(&scope).unwrap())
}

fn write_config(home: &Path, text: &str) {
    let path = home.join("profiles/default/config.toml");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn json_of(home: &Path, args: &[&str]) -> Value {
    let mut full = vec!["--json"];
    full.extend_from_slice(args);
    serde_json::from_str(&ok(&run(home, &full))).unwrap()
}

fn codex_file(home: &Path) -> PathBuf {
    let part = |v: &Value| URL_SAFE_NO_PAD.encode(v.to_string());
    let claims = json!({"https://api.openai.com/auth": {"chatgpt_account_id": "acct-q", "chatgpt_plan_type": "pro"}});
    let access = format!("{}.{}.sig", part(&json!({"alg": "none"})), part(&claims));
    let path = home.join("codex-auth.json");
    let tokens = json!({"access_token": access, "refresh_token": "rt-quota-0123456789"});
    std::fs::write(&path, json!({"tokens": tokens}).to_string()).unwrap();
    path
}

#[test]
fn codex_quota_shown() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let server = runtime.block_on(MockServer::start());
    runtime.block_on(
        Mock::given(method("GET"))
            .and(path("/backend-api/wham/usage"))
            .and(header("chatgpt-account-id", "acct-q"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "plan_type": "pro",
                "rate_limit": {
                    "primary_window": {"used_percent": 72.0, "limit_window_seconds": 18_000, "reset_at": 1_793_973_600},
                    "secondary_window": {"used_percent": 30.0, "limit_window_seconds": 604_800, "reset_at": 1_794_400_000}
                }
            })))
            .expect(1)
            .mount(&server),
    );
    let home = temp_home();
    write_config(
        home.path(),
        &format!(
            "[providers.openai-codex]\nbase_url = \"{}/backend-api/codex\"\nkey_hosts = [\"127.0.0.1\"]\n",
            server.uri()
        ),
    );
    let file = codex_file(home.path());
    ok(&run(
        home.path(),
        &[
            "auth",
            "import",
            "codex-cli",
            "--path",
            file.to_str().unwrap(),
            "--label",
            "personal",
        ],
    ));
    let text = ok(&run(home.path(), &["auth", "status", "openai-codex"]));
    assert!(text.contains("plan pro"), "{text}");
    assert!(
        text.contains("5h 72% (resets 2026-11-06T14:00:00Z)"),
        "{text}"
    );
    assert!(text.contains("weekly 30%"), "{text}");
    // A second look within the minute reads the stored probe.
    let doc = json_of(home.path(), &["auth", "status", "openai-codex"]);
    let quota = &doc["quota"]["personal"];
    assert_eq!(quota["plan"], "pro");
    assert_eq!(quota["windows"][0]["used_percent"], 72.0);
    assert_eq!(quota["windows"][1]["name"], "weekly");
    runtime.block_on(server.verify());
}

#[test]
fn ledger_distinguishes_subscription_calls() {
    let home = temp_home();
    write_config(home.path(), "");
    let store = store(home.path());
    let id = store
        .create_session(&NewSession::new(Source::Cli))
        .unwrap()
        .id;
    let recorder = UsageRecorder::new(Arc::clone(&store), Arc::new(ConfigPrices::default()));
    let calls = [
        ("openai-codex", "subscription:openai-codex/personal", None),
        ("openai-codex", "subscription:openai-codex/personal", None),
        ("openai-codex", "subscription:openai-codex/personal", None),
        ("openrouter", "api_key:openrouter/default", Some(0.01)),
        ("openrouter", "api_key:openrouter/default", Some(0.02)),
    ];
    for (provider, source, cost) in calls {
        recorder
            .record_at(
                Call {
                    tokens: Tokens {
                        input: 100,
                        output: 10,
                        ..Tokens::default()
                    },
                    reported_usd: cost,
                    billing_source: source.to_owned(),
                    ..Call::new(id, "main", provider, "m")
                },
                Timestamp::now(),
            )
            .unwrap();
    }
    let doc = json_of(home.path(), &["usage", "--session", &id.to_string()]);
    let groups = doc["profiles"][0]["billing_sources"].as_array().unwrap();
    assert_eq!(groups.len(), 2, "{doc}");
    assert_eq!(groups[0]["key"], "api_key:openrouter/default");
    assert_eq!(groups[0]["calls"], 2);
    assert!(groups[0].get("cost_status").is_none());
    assert_eq!(groups[1]["key"], "subscription:openai-codex/personal");
    assert_eq!(groups[1]["calls"], 3);
    assert_eq!(groups[1]["cost_status"], "subscription");
    let rows = store.usage_events(id).unwrap();
    assert!(
        rows.iter()
            .filter(|r| r.billing_source.starts_with("subscription:"))
            .all(|r| r.cost_status.as_str() == "subscription")
    );
    let by = json_of(
        home.path(),
        &["usage", "--session", &id.to_string(), "--by", "billing"],
    );
    assert_eq!(by["by"], "billing");
    assert_eq!(by["profiles"][0]["rows"][1]["cost_status"], "subscription");
}

#[test]
fn repeated_overloads_lower_the_score() {
    let home = temp_home();
    write_config(home.path(), "");
    let store = store(home.path());
    for _ in 0..10 {
        store
            .record_provider_call(&CallSample {
                provider: "anthropic".to_owned(),
                model: "claude-sonnet-4-5".to_owned(),
                credential: "secret:ANTHROPIC_API_KEY".to_owned(),
                ts: Timestamp::now(),
                class: Some("overloaded".to_owned()),
                ttft_ms: None,
                output_tps: None,
            })
            .unwrap();
    }
    let doc = json_of(home.path(), &["providers", "health"]);
    let route = &doc["routes"][0];
    assert_eq!(route["model"], "claude-sonnet-4-5");
    assert!(route["score"].as_f64().unwrap() < 0.2, "{doc}");
    assert_eq!(route["errors"]["overloaded"], 10);
    assert_eq!(route["calls"], 10);
    let text = ok(&run(home.path(), &["providers", "health"]));
    assert!(text.contains("overloaded 10"), "{text}");
}

/// Scenario: Missing external CLI (spec: model-providers, Plugin OAuth and
/// external-process providers): `claude` is on neither `PATH` nor the
/// user-local bin directories, so the session does not start.
#[test]
fn a_session_on_a_missing_agent_cli_names_it() {
    let home = temp_home();
    write_config(
        home.path(),
        "[model]\nprovider = \"agent-cli\"\ndefault = \"default\"\n\n\
         [providers.agent-cli]\nauth = \"external_process\"\ncommand = \"claude\"\n",
    );
    let empty = home.path().join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let out = common::command(home.path())
        .env("PATH", &empty)
        .env("HOME", &empty)
        .env("USERPROFILE", &empty)
        .args(["-z", "hi"])
        .output()
        .unwrap();
    let err = common::stderr(&out);
    assert_eq!(out.status.code(), Some(69), "{err}");
    assert!(
        err.contains("agent CLI 'claude', which is not installed"),
        "{err}"
    );
}

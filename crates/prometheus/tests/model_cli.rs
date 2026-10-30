//! `prometheus model explain` and provider config validation (spec:
//! model-providers, Requested provider precedence without ambient
//! environment; Deterministic explainable resolution; Auto provider
//! detection; Unknown and disabled providers; Provider descriptor
//! contract; Capability-aware route validation).

mod common;

use std::path::Path;
use std::process::{Command, Output, Stdio};

use serde_json::Value;

use common::{command, stderr, stdout, temp_home};

fn prometheus(home: &Path) -> Command {
    let mut cmd = command(home);
    cmd.stdin(Stdio::null());
    cmd
}

fn write_config(home: &Path, text: &str) {
    let profile = home.join("profiles/default");
    std::fs::create_dir_all(&profile).unwrap();
    std::fs::write(profile.join("config.toml"), text).unwrap();
}

fn explain(mut cmd: Command, args: &[&str]) -> (Output, Option<Value>) {
    let out = cmd
        .args(["--json", "model", "explain"])
        .args(args)
        .output()
        .unwrap();
    let json = serde_json::from_str(&stdout(&out)).ok();
    (out, json)
}

#[test]
fn stale_shell_export_is_ignored() {
    let home = temp_home();
    write_config(home.path(), "[model]\nprovider = \"anthropic\"\n");
    let mut cmd = prometheus(home.path());
    cmd.env("PMT_INFERENCE_PROVIDER", "openrouter")
        .env("OPENROUTER_API_KEY", "sk-or-v1-stale-0123456789abcdef")
        .env("ANTHROPIC_API_KEY", "sk-ant-api03-test-0123456789abcdef");
    let (out, json) = explain(cmd, &[]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let json = json.unwrap();
    assert_eq!(json["schema"], "prometheus.model_explain.v1");
    assert_eq!(json["route"]["provider"], "anthropic");
    assert_eq!(json["route"]["credential"], "secret:ANTHROPIC_API_KEY");
    assert_eq!(json["route"]["wire_format"], "anthropic_messages");
    assert_eq!(json["route"]["billing_source"], "api_key:anthropic");
}

#[test]
fn explain_prints_every_step_and_no_key_material() {
    let home = temp_home();
    let secret = "sk-or-v1-secret-value-0123456789abcdef";
    let mut cmd = prometheus(home.path());
    cmd.env("OPENROUTER_API_KEY", secret);
    let (out, json) = explain(cmd, &["--provider", "or"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let json = json.unwrap();
    let steps: Vec<&str> = json["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["step"].as_str().unwrap())
        .collect();
    assert_eq!(
        steps,
        [
            "requested",
            "provider",
            "enabled",
            "endpoint",
            "wire_format",
            "credential",
            "model",
            "capability"
        ]
    );
    assert_eq!(json["route"]["provider"], "openrouter");
    assert_eq!(json["route"]["credential"], "secret:OPENROUTER_API_KEY");
    assert!(!stdout(&out).contains(secret) && !stderr(&out).contains(secret));
    // Text form shows the same trace.
    let mut text = prometheus(home.path());
    text.env("OPENROUTER_API_KEY", secret);
    let out = text
        .args(["model", "explain", "--provider", "or"])
        .output()
        .unwrap();
    assert!(
        stdout(&out).contains("credential   ok     secret:OPENROUTER_API_KEY"),
        "{}",
        stdout(&out)
    );
}

#[test]
fn typo_in_provider_suggests_the_closest_id_and_exits_78() {
    let home = temp_home();
    let (out, json) = explain(prometheus(home.path()), &["--provider", "opnrouter"]);
    assert_eq!(out.status.code(), Some(78));
    let error: Value = serde_json::from_str(&stderr(&out)).unwrap();
    assert_eq!(error["error"]["code"], "provider.unknown");
    assert_eq!(
        error["error"]["message"],
        "Unknown provider 'opnrouter'. Did you mean 'openrouter'?"
    );
    // The trace up to the failing step is still printed.
    assert_eq!(json.unwrap()["steps"][1]["outcome"], "failed");
}

#[test]
fn nothing_configured_prints_the_not_connected_message_and_exits_78() {
    let home = temp_home();
    let out = prometheus(home.path())
        .args(["model", "explain"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(78));
    assert!(
        stderr(&out).contains("Prometheus is not connected to any model provider. Run 'prometheus model' or 'prometheus auth add <provider>'."),
        "{}",
        stderr(&out)
    );
}

#[test]
fn disabled_provider_fails_before_credential_resolution() {
    let home = temp_home();
    write_config(home.path(), "[providers.openrouter]\nenabled = false\n");
    let (out, json) = explain(prometheus(home.path()), &["--provider", "openrouter"]);
    assert_eq!(out.status.code(), Some(78));
    assert!(stderr(&out).contains(
        "provider 'openrouter' is disabled in config (providers.openrouter.enabled = false)"
    ));
    let steps = json.unwrap()["steps"].to_string();
    assert!(!steps.contains("\"credential\""));
}

#[test]
fn small_local_window_is_refused() {
    let home = temp_home();
    write_config(
        home.path(),
        "[providers.ollama]\nbase_url = \"http://127.0.0.1:9/v1\"\n[models.\"ollama/qwen3\"]\ncontext_window = 32768\n",
    );
    let (out, _) = explain(
        prometheus(home.path()),
        &["--provider", "ollama", "--model", "qwen3"],
    );
    assert_eq!(out.status.code(), Some(78));
    let error: Value = serde_json::from_str(&stderr(&out)).unwrap();
    assert_eq!(error["error"]["code"], "model.context_too_small");
}

fn validate(home: &Path, config: &str) -> Output {
    write_config(home, config);
    prometheus(home)
        .args(["config", "validate"])
        .output()
        .unwrap()
}

#[test]
fn provider_configuration_is_validated() {
    let home = temp_home();
    let out = validate(
        home.path(),
        "[providers.myproxy]\nbase_url = \"http://llm.example.com/v1\"\n",
    );
    assert_eq!(out.status.code(), Some(78));
    assert!(
        stderr(&out).contains("providers.myproxy.base_url"),
        "{}",
        stderr(&out)
    );
    assert!(stderr(&out).contains("allow_insecure_http"));

    let out = validate(
        home.path(),
        "[providers.ollama]\nbase_url = \"http://127.0.0.1:11434/v1\"\n",
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));

    let out = validate(
        home.path(),
        "[providers.x]\nbase_url = \"https://x.test/v1\"\nwire_format = \"chat\"\n",
    );
    assert_eq!(out.status.code(), Some(78));
    assert!(
        stderr(&out).contains("chat_completions"),
        "{}",
        stderr(&out)
    );

    let out = validate(
        home.path(),
        "[providers.openrouter]\npool_strategy = \"weighted\"\n",
    );
    assert_eq!(out.status.code(), Some(78));
    assert!(stderr(&out).contains("providers.openrouter.pool_strategy"));

    let out = validate(
        home.path(),
        "[[fallback.chain]]\nprovider = \"nope\"\nmodel = \"m\"\n",
    );
    assert_eq!(out.status.code(), Some(78));
    assert!(
        stderr(&out).contains("fallback.chain[0].provider: Unknown provider 'nope'."),
        "{}",
        stderr(&out)
    );

    let out = validate(home.path(), "[providers.mine]\nenabled = true\n");
    assert_eq!(out.status.code(), Some(78));
    assert!(stderr(&out).contains("providers.mine.base_url is required"));
}

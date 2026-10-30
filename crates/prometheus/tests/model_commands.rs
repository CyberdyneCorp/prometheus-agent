//! `prometheus model [set|list|info]`, the picker refusal, `fallback`
//! commands and the launch-only endpoint (spec: model-providers,
//! prometheus model command; Built-in provider catalog; Local server
//! detection and context probes; Context length detection chain;
//! Reasoning effort display; User model aliases; Fallback chain commands;
//! Launch-only ad-hoc endpoint).

mod common;

use std::path::Path;
use std::process::{Command, Output, Stdio};

use serde_json::Value;

use common::{command, files_under, run_cmd_with_stdin, stderr, stdout, temp_home};

const OR_KEY: &str = "sk-or-v1-test-key-0123456789abcdef";

fn prometheus(home: &Path) -> Command {
    let mut cmd = command(home);
    cmd.stdin(Stdio::null());
    cmd
}

fn config_path(home: &Path) -> std::path::PathBuf {
    home.join("profiles/default/config.toml")
}

fn write_config(home: &Path, text: &str) {
    let path = config_path(home);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn config(home: &Path) -> toml::Table {
    toml::from_str(&std::fs::read_to_string(config_path(home)).unwrap_or_default()).unwrap()
}

fn run(home: &Path, args: &[&str]) -> Output {
    prometheus(home)
        .env("OPENROUTER_API_KEY", OR_KEY)
        .args(args)
        .output()
        .unwrap()
}

fn json(out: &Output) -> Value {
    serde_json::from_str(&stdout(out)).unwrap_or_else(|e| panic!("{e}: {}", stdout(out)))
}

#[test]
fn piped_invocation_of_the_picker_exits_2() {
    let home = temp_home();
    let out = run_cmd_with_stdin(command(home.path()).arg("model"), "\n");
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(
        stderr(&out).contains(
            "prometheus model requires a terminal; use 'prometheus model set <provider>/<model>'"
        ),
        "{}",
        stderr(&out)
    );
    assert!(stdout(&out).is_empty());
}

#[test]
fn non_interactive_set_validates_first() {
    let home = temp_home();
    write_config(home.path(), "# mine\n");
    let out = run(home.path(), &["model", "set", "deepseek/deepseek-chat"]);
    assert_eq!(out.status.code(), Some(78), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("prometheus auth add deepseek"),
        "{}",
        stderr(&out)
    );
    assert_eq!(
        std::fs::read_to_string(config_path(home.path())).unwrap(),
        "# mine\n"
    );
}

#[test]
fn set_writes_the_validated_route_and_aliases_stay_aliases() {
    let home = temp_home();
    let out = run(home.path(), &["model", "set", "or/some-model"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let model = config(home.path())["model"].clone();
    assert_eq!(model["provider"].as_str(), Some("openrouter"));
    assert_eq!(model["default"].as_str(), Some("some-model"));

    write_config(
        home.path(),
        "[model.aliases.fav]\nprovider = \"openrouter\"\nmodel = \"anthropic/claude-x\"\n",
    );
    let out = run(home.path(), &["--json", "model", "set", "FAV"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let view = json(&out);
    assert_eq!(view["schema"], "prometheus.model_set.v1");
    assert_eq!(view["model"], "anthropic/claude-x");
    let model = config(home.path())["model"].clone();
    assert_eq!(model["default"].as_str(), Some("FAV"));
    // A new session resolves the alias to its own provider and key.
    let out = run(home.path(), &["--json", "model", "explain"]);
    let explained = json(&out);
    assert_eq!(explained["route"]["provider"], "openrouter");
    assert_eq!(explained["route"]["model"], "anthropic/claude-x");
    assert_eq!(
        explained["route"]["credential"],
        "secret:OPENROUTER_API_KEY"
    );
}

#[test]
fn every_built_in_id_listed_without_secret_values() {
    let home = temp_home();
    let out = run(home.path(), &["--json", "model", "list", "--providers"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(!stdout(&out).contains(OR_KEY));
    let doc = json(&out);
    assert_eq!(doc["schema"], "prometheus.providers.v1");
    let rows = doc["providers"].as_array().unwrap();
    for descriptor in pmt_providers::builtin::descriptors() {
        let row = rows
            .iter()
            .find(|r| r["id"] == descriptor.id)
            .unwrap_or_else(|| panic!("{} missing", descriptor.id));
        assert_eq!(row["aliases"], serde_json::json!(descriptor.aliases));
        assert_eq!(row["wire_format"], descriptor.wire_label());
        assert_eq!(
            row["secret_names"],
            serde_json::json!(descriptor.secret_names)
        );
        let endpoint = descriptor
            .base_url
            .clone()
            .or(descriptor.endpoint_note.clone())
            .unwrap();
        assert_eq!(row["endpoint"], endpoint, "{}", descriptor.id);
    }
}

#[test]
fn info_shows_the_pinned_window_and_the_clamped_effort() {
    let home = temp_home();
    write_config(
        home.path(),
        "[model]\nprovider = \"openrouter\"\ndefault = \"x\"\n\
         [agent]\nreasoning_effort = \"ultra\"\n\
         [models.\"openrouter/x\"]\ncontext_window = 200000\nsupports_reasoning = true\nmax_reasoning_effort = \"high\"\n",
    );
    let out = run(home.path(), &["--json", "model", "info"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let info = json(&out);
    assert_eq!(info["schema"], "prometheus.model_info.v1");
    assert_eq!(info["context_window"], 200_000);
    assert_eq!(info["context_source"], "pinned");
    assert_eq!(info["reasoning"]["sent"], "high");
    assert_eq!(info["reasoning"]["clamped"], true);
    let line = info["status_line"].as_str().unwrap();
    assert!(line.contains("ctx 200000 (pinned)"), "{line}");
    assert!(
        line.contains("reasoning high (clamped from ultra)"),
        "{line}"
    );
    let out = run(home.path(), &["model", "info"]);
    assert!(stdout(&out).contains("200000 (pinned)"), "{}", stdout(&out));
}

#[test]
fn list_shows_models_with_their_windows() {
    let home = temp_home();
    let out = run(
        home.path(),
        &["--json", "model", "list", "--provider", "anthropic"],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let doc = json(&out);
    assert_eq!(doc["schema"], "prometheus.models.v1");
    let sonnet = doc["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["model"] == "claude-sonnet-4-5")
        .unwrap();
    assert_eq!(sonnet["context_window"], 200_000);
    assert_eq!(sonnet["supports_tools"], true);
}

#[tokio::test(flavor = "multi_thread")]
async fn root_url_normalised_and_saved_when_the_endpoint_is_tested() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            serde_json::json!({"data": [{"id": "qwen3", "max_model_len": 131_072}]}),
        ))
        .mount(&server)
        .await;
    let home = temp_home();
    write_config(
        home.path(),
        &format!("[providers.lmstudio]\nbase_url = \"{}\"\n", server.uri()),
    );
    let out = run(home.path(), &["model", "set", "lmstudio/qwen3"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let saved = config(home.path())["providers"]["lmstudio"]["base_url"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(saved, format!("{}/v1", server.uri()));
    // Chat requests go to the same prefix.
    let out = run(home.path(), &["--json", "model", "explain"]);
    assert_eq!(json(&out)["route"]["base_url"], saved);
}

#[test]
fn fallback_commands_edit_the_chain_and_refuse_duplicates() {
    let home = temp_home();
    let out = run(home.path(), &["fallback", "add", "openrouter/x"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let before = std::fs::read_to_string(config_path(home.path())).unwrap();
    let out = run(home.path(), &["fallback", "add", "or/x"]);
    assert_eq!(out.status.code(), Some(75), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("already in the fallback chain"),
        "{}",
        stderr(&out)
    );
    assert_eq!(
        std::fs::read_to_string(config_path(home.path())).unwrap(),
        before
    );
    // A route without a credential is refused before writing.
    let out = run(home.path(), &["fallback", "add", "deepseek/deepseek-chat"]);
    assert_eq!(out.status.code(), Some(78), "{}", stderr(&out));
    let out = run(home.path(), &["fallback", "add", "openrouter/y"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let out = run(home.path(), &["--json", "fallback", "list"]);
    let doc = json(&out);
    assert_eq!(doc["schema"], "prometheus.fallback.v1");
    let models: Vec<&str> = doc["chain"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["model"].as_str().unwrap())
        .collect();
    assert_eq!(models, ["x", "y"]);
    let out = run(home.path(), &["fallback", "remove", "or/x"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let out = run(home.path(), &["fallback", "remove", "5"]);
    assert_eq!(out.status.code(), Some(66), "{}", stderr(&out));
    let out = run(home.path(), &["fallback", "clear"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(
        config(home.path())
            .get("fallback")
            .is_none_or(|f| f.get("chain").is_none())
    );
    let out = run_cmd_with_stdin(command(home.path()).args(["fallback", "add"]), "");
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
}

#[test]
fn ad_hoc_key_never_reaches_disk() {
    let home = temp_home();
    let key = "sk-adhoc-launch-only-0123456789abcdef";
    let out = run_cmd_with_stdin(
        command(home.path()).args([
            "--json",
            "model",
            "explain",
            "--provider",
            "custom",
            "--base-url",
            "https://llm.example.com/v1",
            "--api-key-stdin",
            "--model",
            "m",
        ]),
        &format!("{key}\n"),
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let route = json(&out)["route"].clone();
    assert_eq!(route["credential"], "flag");
    assert_eq!(route["base_url"], "https://llm.example.com/v1");
    assert!(!stdout(&out).contains(key) && !stderr(&out).contains(key));
    for file in files_under(home.path()) {
        let bytes = std::fs::read(&file).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            !text.contains(key),
            "{} holds the ad-hoc key",
            file.display()
        );
    }
    // `--api-key-stdin` needs `--base-url`.
    let out = run_cmd_with_stdin(
        command(home.path()).args(["model", "explain", "--api-key-stdin"]),
        "k\n",
    );
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
}

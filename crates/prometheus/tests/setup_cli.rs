//! `prometheus setup` scripted on a temporary home (spec: cli, Setup
//! wizard; Onboarding state; configuration-and-profiles, Secrets are not
//! configuration).

mod common;

use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use common::{command, ok, run, run_cmd_with_stdin, run_with_stdin, stderr, temp_home};

const KEY: &str = "sk-ant-test-0123456789abcdef";

fn config(home: &Path) -> String {
    std::fs::read_to_string(home.join("profiles/default/config.toml")).unwrap()
}

/// Provider names come from the picker list; a name is accepted too.
#[test]
fn the_wizard_writes_config_and_the_key_to_the_secrets_store() {
    let home = temp_home();
    let out = run_with_stdin(
        home.path(),
        &["setup", "model"],
        &format!("anthropic\nclaude-sonnet-4-5\n{KEY}\n"),
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let text = config(home.path());
    assert!(text.contains("provider = \"anthropic\""), "{text}");
    assert!(text.contains("default = \"claude-sonnet-4-5\""), "{text}");
    assert!(!text.contains(KEY), "the key never lands in config.toml");
    let listed = ok(&run(home.path(), &["secrets", "list"]));
    assert!(listed.contains("ANTHROPIC_API_KEY"), "{listed}");
    let secrets =
        std::fs::read_to_string(home.path().join("profiles/default/secrets.toml")).unwrap();
    assert!(secrets.contains(KEY), "stored in the secrets store");
}

/// Scenario: Re-run is idempotent.
#[test]
fn accepting_every_default_leaves_config_byte_identical() {
    let home = temp_home();
    ok(&run_with_stdin(
        home.path(),
        &["setup", "model"],
        &format!("anthropic\nclaude-sonnet-4-5\n{KEY}\n"),
    ));
    let before = config(home.path());
    ok(&run_with_stdin(home.path(), &["setup", "model"], "\n"));
    assert_eq!(config(home.path()), before);
    let all = run_with_stdin(home.path(), &["setup"], "\n\n\n\n\n\n");
    assert_eq!(all.status.code(), Some(0), "{}", stderr(&all));
    assert!(
        stderr(&all).contains("ANTHROPIC_API_KEY (anthropic): set"),
        "{}",
        stderr(&all)
    );
    assert_eq!(config(home.path()), before);
}

#[test]
fn non_interactive_takes_set_values_and_env_secrets_or_exits_78() {
    let home = temp_home();
    let args = [
        "--set",
        "model.provider=anthropic",
        "--set",
        "model.default=claude-sonnet-4-5",
        "setup",
        "model",
        "--non-interactive",
    ];
    let none = run(home.path(), &["setup", "model", "--non-interactive"]);
    assert_eq!(none.status.code(), Some(78));
    assert!(
        stderr(&none).contains("model.provider"),
        "{}",
        stderr(&none)
    );
    let missing = run(home.path(), &args);
    assert_eq!(missing.status.code(), Some(78), "{}", stderr(&missing));
    assert!(
        stderr(&missing).contains("ANTHROPIC_API_KEY"),
        "{}",
        stderr(&missing)
    );
    let mut cmd = command(home.path());
    cmd.args(args).env("PMT_SECRET_ANTHROPIC_API_KEY", KEY);
    ok(&run_cmd_with_stdin(&mut cmd, ""));
    assert!(config(home.path()).contains("provider = \"anthropic\""));
    let secrets =
        std::fs::read_to_string(home.path().join("profiles/default/secrets.toml")).unwrap();
    assert!(secrets.contains(KEY));
}

#[test]
fn gateway_setup_waits_for_the_gateway() {
    let home = temp_home();
    let out = run(home.path(), &["setup", "gateway"]);
    assert_eq!(out.status.code(), Some(69), "{}", stderr(&out));
}

fn jwt(claims: &Value) -> String {
    let part = |v: &Value| URL_SAFE_NO_PAD.encode(v.to_string());
    format!("{}.{}.sig", part(&json!({"alg": "none"})), part(claims))
}

/// The subscription choice signs in (device code against a local fake)
/// and makes the provider the profile's.
#[tokio::test(flavor = "multi_thread")]
async fn the_subscription_choice_signs_in_and_sets_the_provider() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/accounts/deviceauth/usercode"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"device_auth_id": "dauth-setup", "user_code": "SETUP-CODE", "interval": "0"}),
        ))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/accounts/deviceauth/token"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                json!({"authorization_code": "ac-setup", "code_verifier": "v-setup"}),
            ),
        )
        .mount(&server)
        .await;
    let access = jwt(
        &json!({"https://api.openai.com/auth": {"chatgpt_account_id": "acct-setup", "chatgpt_plan_type": "plus"}}),
    );
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"access_token": access, "refresh_token": "rt-setup-0123456789abcdef", "expires_in": 3600}),
        ))
        .mount(&server)
        .await;
    let home = temp_home();
    let file = home.path().join("profiles/default/config.toml");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(
        &file,
        format!(
            "[providers.openai-codex.oauth]\nauthorize_url = \"{0}/oauth/authorize\"\ntoken_url = \"{0}/oauth/token\"\n\
             device_authorization_url = \"{0}/api/accounts/deviceauth/usercode\"\n\
             device_poll_url = \"{0}/api/accounts/deviceauth/token\"\nallowed_hosts = [\"127.0.0.1\"]\n",
            server.uri()
        ),
    )
    .unwrap();
    let out = run_with_stdin(home.path(), &["setup", "model"], "1\n");
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(stderr(&out).contains("SETUP-CODE"), "{}", stderr(&out));
    assert!(config(home.path()).contains("provider = \"openai-codex\""));
    let listed: Value =
        serde_json::from_str(&ok(&run(home.path(), &["auth", "list", "--json"]))).unwrap();
    assert_eq!(listed["credentials"][0]["provider"], "openai-codex");
}

#[test]
fn reset_onboarding_clears_the_state() {
    let home = temp_home();
    let out = ok(&run(home.path(), &["setup", "--reset-onboarding"]));
    assert!(out.contains("onboarding reset"), "{out}");
}

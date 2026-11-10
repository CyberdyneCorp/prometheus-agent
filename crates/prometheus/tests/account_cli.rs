//! Account usage and the tool gateway end to end (spec: model-providers,
//! Account usage windows command; Subscription tool gateway routing).

mod common;

use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jiff::Timestamp;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use common::{ok, run, run_with_stdin, stderr, stdout, temp_home};

fn write_config(home: &Path, text: &str) {
    let file = home.join("profiles/default/config.toml");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(file, text).unwrap();
}

fn jwt(claims: &Value) -> String {
    let part = |v: &Value| URL_SAFE_NO_PAD.encode(v.to_string());
    format!("{}.{}.sig", part(&json!({"alg": "none"})), part(claims))
}

fn chatgpt_sign_in(home: &Path) {
    let access = jwt(&json!({
        "exp": Timestamp::now().as_second() + 3600,
        "https://api.openai.com/auth": {"chatgpt_account_id": "acct-1", "chatgpt_plan_type": "pro"}
    }));
    let file = home.join("codex-auth.json");
    std::fs::write(
        &file,
        json!({"tokens": {"access_token": access, "refresh_token": "rt-0123456789abcdef"}})
            .to_string(),
    )
    .unwrap();
    ok(&run(
        home,
        &[
            "auth",
            "import",
            "codex-cli",
            "--path",
            file.to_str().unwrap(),
        ],
    ));
}

/// Scenario: JSON for scripts.
#[tokio::test(flavor = "multi_thread")]
async fn usage_json_carries_the_plans_two_windows() {
    let server = MockServer::start().await;
    let reset = Timestamp::now().as_second() + 7200;
    Mock::given(method("GET"))
        .and(path("/backend-api/wham/usage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "plan_type": "pro",
            "rate_limit": {
                "primary_window": {"used_percent": 42, "limit_window_seconds": 18_000, "reset_at": reset},
                "secondary_window": {"used_percent": 7, "limit_window_seconds": 604_800, "reset_at": reset + 86400}
            }
        })))
        .mount(&server)
        .await;
    let home = temp_home();
    write_config(
        home.path(),
        &format!(
            "[providers.openai-codex]\nbase_url = \"{}/backend-api/codex\"\nkey_hosts = [\"127.0.0.1\"]\n",
            server.uri()
        ),
    );
    chatgpt_sign_in(home.path());
    let out = run(
        home.path(),
        &["auth", "usage", "--provider", "openai-codex", "--json"],
    );
    let doc: Value = serde_json::from_str(&ok(&out)).unwrap();
    assert_eq!(doc["provider"], "openai-codex");
    assert_eq!(doc["plan"], "pro");
    let windows = doc["windows"].as_array().unwrap();
    assert_eq!(windows.len(), 2, "{doc}");
    for window in windows {
        assert!(window["used_percent"].is_number());
        assert!(window["resets_at"].is_string());
    }
    assert_eq!(doc["source"], "subscription:openai-codex/default");
}

/// Scenario: Nothing to report.
#[test]
fn a_provider_without_a_usage_endpoint_exits_1_naming_it() {
    let home = temp_home();
    ok(&run_with_stdin(
        home.path(),
        &["auth", "add", "deepseek"],
        "sk-deepseek-0123456789\n",
    ));
    let out = run(home.path(), &["auth", "usage", "--provider", "deepseek"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(stderr(&out).contains("deepseek"), "{}", stderr(&out));
    assert!(stdout(&out).is_empty());
}

#[test]
fn portal_tools_routes_categories_through_the_gateway() {
    let home = temp_home();
    ok(&run(
        home.path(),
        &[
            "portal",
            "tools",
            "--route",
            "image_generation",
            "--route",
            "browser",
        ],
    ));
    let views: Value =
        serde_json::from_str(&ok(&run(home.path(), &["portal", "tools", "--json"]))).unwrap();
    let of = |name: &str| {
        views
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["category"] == name)
            .unwrap()
            .clone()
    };
    // Not signed in: the stored selection is kept and named, never rerouted.
    assert_eq!(of("image_generation")["selection"], "nous");
    assert_eq!(of("image_generation")["route"], "error");
    assert!(
        of("image_generation")["detail"]
            .as_str()
            .unwrap()
            .contains("image_gen.provider")
    );
    assert_eq!(of("browser")["selection"], "nous");
    let bad = run(home.path(), &["portal", "tools", "--route", "teleport"]);
    assert_eq!(bad.status.code(), Some(2), "{}", stderr(&bad));
}

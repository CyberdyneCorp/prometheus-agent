//! Credential pool maintenance end to end (spec: model-providers,
//! Credential pool maintenance commands; Credential pool seeding, custom
//! endpoint pools and child sharing).

mod common;

use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jiff::{SignedDuration, Timestamp};
use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
use serde_json::{Value, json};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use common::{ok, run, run_with_stdin, stderr, temp_home};

fn set_secret(home: &Path, name: &str, value: &str) {
    ok(&run_with_stdin(
        home,
        &["secrets", "set", name],
        &format!("{value}\n"),
    ));
}

fn json_of(home: &Path, args: &[&str]) -> Value {
    serde_json::from_str(&ok(&run(home, args))).unwrap()
}

fn rows<'a>(doc: &'a Value, provider: &str) -> Vec<&'a Value> {
    doc["credentials"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["provider"] == provider)
        .collect()
}

fn store(home: &Path) -> pmt_store::Store {
    let root = Home::open(home).unwrap();
    let profile = root.profile(&ProfileName::default_profile()).unwrap();
    let scope = ProfileScope::open(profile, &Overrides::default(), Launch::Other).unwrap();
    pmt_store::Store::open(&scope).unwrap()
}

/// Scenario: Numbered siblings join the pool.
#[test]
fn numbered_siblings_are_seeded_members_and_auth_add_keys_are_added() {
    let home = temp_home();
    set_secret(home.path(), "OPENROUTER_API_KEY", "sk-or-first-0123456789");
    set_secret(
        home.path(),
        "OPENROUTER_API_KEY_2",
        "sk-or-second-0123456789",
    );
    ok(&run_with_stdin(
        home.path(),
        &[
            "auth",
            "add",
            "openrouter",
            "--label",
            "work",
            "--priority",
            "0",
        ],
        "sk-or-work-0123456789\n",
    ));
    let doc = json_of(home.path(), &["auth", "list", "--json"]);
    let pool: Vec<(&str, &str)> = rows(&doc, "openrouter")
        .iter()
        .map(|r| (r["label"].as_str().unwrap(), r["pool"].as_str().unwrap()))
        .collect();
    // `--priority 0` put the added key first.
    assert_eq!(
        pool,
        [("work", "added"), ("default", "seeded"), ("2", "seeded")]
    );

    let order = json_of(
        home.path(),
        &["auth", "priority", "openrouter", "default", "0", "--json"],
    );
    assert_eq!(order["order"], json!(["default", "work", "2"]));
    // A label wins over a position: `2` is the member labelled 2.
    let order = json_of(
        home.path(),
        &["auth", "priority", "openrouter", "2", "1", "--json"],
    );
    assert_eq!(order["order"], json!(["default", "2", "work"]));
}

#[test]
fn reset_clears_a_bench_in_state_db_and_status_lists_it_ok() {
    let home = temp_home();
    set_secret(home.path(), "OPENROUTER_API_KEY", "sk-or-first-0123456789");
    let until = Timestamp::now() + SignedDuration::from_hours(1);
    store(home.path())
        .bench_credential(
            "openrouter",
            "secret:OPENROUTER_API_KEY",
            Some(until),
            Some("billing"),
        )
        .unwrap();
    let before = json_of(home.path(), &["auth", "status", "openrouter", "--json"]);
    assert_eq!(before["credentials"][0]["status"], "benched");
    let out = run(home.path(), &["auth", "reset", "openrouter"]);
    ok(&out);
    assert!(
        stderr(&out).contains("cleared the benches of 1"),
        "{}",
        stderr(&out)
    );
    let after = json_of(home.path(), &["auth", "status", "openrouter", "--json"]);
    assert_eq!(after["credentials"][0]["status"], "ok");
}

fn jwt(claims: &Value) -> String {
    let part = |v: &Value| URL_SAFE_NO_PAD.encode(v.to_string());
    format!("{}.{}.sig", part(&json!({"alg": "none"})), part(claims))
}

/// Imports a ChatGPT sign-in whose access token expires in 30 seconds.
fn expiring_sign_in(home: &Path) {
    let exp = Timestamp::now().as_second() + 30;
    let access = jwt(&json!({
        "exp": exp,
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

/// Scenario: Status is a peek.
#[tokio::test(flavor = "multi_thread")]
async fn status_reports_an_expiring_token_without_refreshing_it() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "new-access", "refresh_token": "rt-new", "expires_in": 3600
        })))
        .mount(&server)
        .await;
    let home = temp_home();
    // No request may leave for the real provider: its endpoint is the mock
    // too (the quota probe of `auth status` finds nothing there).
    let config = format!(
        "[providers.openai-codex]\nbase_url = \"{0}/codex\"\n\
         [providers.openai-codex.oauth]\ntoken_url = \"{0}/oauth/token\"\nallowed_hosts = [\"auth.openai.com\", \"127.0.0.1\"]\n",
        server.uri()
    );
    std::fs::create_dir_all(home.path().join("profiles/default")).unwrap();
    std::fs::write(home.path().join("profiles/default/config.toml"), &config).unwrap();
    expiring_sign_in(home.path());

    let doc = json_of(home.path(), &["auth", "status", "openai-codex", "--json"]);
    assert!(doc["credentials"][0]["expires_at"].is_string(), "{doc}");
    let list = run(home.path(), &["auth", "list"]);
    assert!(
        common::stdout(&list).contains("expires"),
        "{}",
        common::stdout(&list)
    );
    let token_calls = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/oauth/token")
        .count();
    assert_eq!(token_calls, 0, "a peek never refreshes");

    // `auth refresh` does, and `auth logout` removes the sign-in.
    ok(&run(home.path(), &["auth", "refresh", "openai-codex"]));
    let token_calls = server.received_requests().await.unwrap().len();
    assert_eq!(token_calls, 1);
    ok(&run(home.path(), &["auth", "logout", "openai-codex"]));
    let doc = json_of(home.path(), &["auth", "list", "--json"]);
    assert!(rows(&doc, "openai-codex").is_empty(), "{doc}");
}

#[test]
fn auth_without_a_subcommand_off_a_terminal_lists_the_credentials() {
    let home = temp_home();
    set_secret(home.path(), "DEEPSEEK_API_KEY", "sk-deepseek-0123456789");
    let out = run(home.path(), &["auth"]);
    assert!(common::stdout(&ok_out(&out)).contains("deepseek  default  api_key"));
}

fn ok_out(out: &std::process::Output) -> std::process::Output {
    assert!(out.status.success(), "{}", stderr(out));
    out.clone()
}

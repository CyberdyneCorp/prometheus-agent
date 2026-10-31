//! Borrowed CLI logins (spec: model-providers, Borrowed CLI logins).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use jiff::SignedDuration;
use pmt_core::error::Error;
use pmt_providers::auth::borrowed::{
    Announcements, Borrowed, ExternalLogin, FileStore, LoginStore,
};
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::accounts::{Setup, bearers, caller_with};
use crate::idp::{Action, Idp};
use crate::support::{request, sse};

/// A login store that counts reads.
#[derive(Debug)]
struct Counting {
    inner: FileStore,
    reads: Arc<AtomicUsize>,
}

impl LoginStore for Counting {
    fn read(&self) -> Result<Option<String>, Error> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.inner.read()
    }

    fn write(&self, text: &str) -> Result<(), Error> {
        self.inner.write(text)
    }
}

fn claude_file(user_home: &Path, access: &str) -> PathBuf {
    let dir = user_home.join(".claude");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join(".credentials.json");
    let login = json!({"claudeAiOauth": {
        "accessToken": access, "refreshToken": "rt-claude-code",
        "expiresAt": 4_102_444_800_000_i64, "scopes": ["user:inference"],
        "subscriptionType": "max"
    }});
    std::fs::write(&file, login.to_string()).unwrap();
    file
}

fn claude_config(token_url: &str, inference: &str) -> String {
    format!(
        "[providers.anthropic]\nbase_url = \"{inference}\"\nkey_hosts = [\"127.0.0.1\"]\n\
         wire_format = \"anthropic_messages\"\n\
         [providers.anthropic.oauth]\ntoken_url = \"{token_url}\"\nallowed_hosts = [\"127.0.0.1\"]\n"
    )
}

async fn token_idp() -> Idp {
    Idp::start(|req, _| match req.path.as_str() {
        "/token" => Action::Reply(
            200,
            json!({"access_token": "at-new", "refresh_token": "rt-new", "expires_in": 3600}),
        ),
        _ => Action::Reply(404, json!({})),
    })
    .await
}

async fn ok_for(server: &MockServer, url_path: &str, token: &str) {
    Mock::given(method("POST"))
        .and(path(url_path))
        .and(header("authorization", format!("Bearer {token}").as_str()))
        .respond_with(sse(if url_path == "/v1/messages" {
            "anthropic_tool_call.sse"
        } else {
            "responses_tool_call.sse"
        }))
        .mount(server)
        .await;
}

async fn otherwise_401(server: &MockServer) {
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(401).set_body_json(json!({"error": {"message": "expired"}})),
        )
        .with_priority(10)
        .mount(server)
        .await;
}

#[tokio::test]
async fn owned_grant_beats_borrowed_login_which_is_never_read() {
    let server = MockServer::start().await;
    let setup = Setup::new(
        &claude_config("http://127.0.0.1:9/token", &server.uri()),
        &[],
    );
    setup.sign_in(
        "anthropic",
        "default",
        "at-owned",
        "rt-owned",
        SignedDuration::from_hours(5),
    );
    let reads = Arc::new(AtomicUsize::new(0));
    let file = claude_file(&setup.user_home(), "at-claude-code");
    let counting = Counting {
        inner: FileStore::new(file),
        reads: reads.clone(),
    };
    let borrowed = Borrowed::with(vec![ExternalLogin::claude_code(Box::new(counting))]);
    let accounts = setup.accounts(borrowed);
    let route = setup.route(&accounts, "anthropic").await.unwrap();
    assert_eq!(
        route.credential.describe(),
        "subscription:anthropic/default"
    );
    assert_eq!(
        reads.load(Ordering::SeqCst),
        0,
        "Claude Code's store was read"
    );
}

#[tokio::test]
async fn borrowing_disabled_never_reads_and_fails_missing_credential() {
    let server = MockServer::start().await;
    ok_for(&server, "/v1/messages", "at-claude-code").await;
    let setup = Setup::new(
        &claude_config("http://127.0.0.1:9/token", &server.uri()),
        &[],
    );
    let user_home = setup.user_home();
    claude_file(&user_home, "at-claude-code");
    let off = Borrowed::discover(false, Some(&user_home));
    assert!(off.login("anthropic").is_none() && off.login("openai-codex").is_none());
    let err = setup
        .route(&setup.accounts(off), "anthropic")
        .await
        .unwrap_err();
    assert_eq!(err.code().code, "provider.missing_credential");
    // Enabled (the default), the same login is borrowed in place.
    let accounts = setup.accounts(Borrowed::discover(true, Some(&user_home)));
    let route = setup.route(&accounts, "anthropic").await.unwrap();
    assert_eq!(
        route.credential.describe(),
        "subscription:anthropic/borrowed:claude-code"
    );
    assert_eq!(
        route.billing_source(),
        "subscription:anthropic/borrowed:claude-code"
    );
    caller_with(&accounts)
        .send(&route, &request("hi"), None)
        .await
        .unwrap();
    assert_eq!(bearers(&server).await, ["at-claude-code"]);
    assert!(
        setup.credentials.store().names().unwrap().is_empty(),
        "nothing is copied into the secrets store"
    );
    let mut notices = Announcements::default();
    let first = notices.first_use(&route.credential).unwrap();
    assert!(
        first.contains("Claude Code") && first.contains("borrowed:claude-code"),
        "{first}"
    );
    assert!(
        notices.first_use(&route.credential).is_none(),
        "once per session"
    );
}

#[tokio::test]
async fn an_owned_401_never_rotates_the_borrowed_login() {
    let idp = token_idp().await;
    let server = MockServer::start().await;
    ok_for(&server, "/v1/messages", "at-new").await;
    otherwise_401(&server).await;
    let setup = Setup::new(&claude_config(&idp.url("/token"), &server.uri()), &[]);
    setup.sign_in(
        "anthropic",
        "default",
        "at-owned",
        "rt-owned",
        SignedDuration::from_hours(5),
    );
    let user_home = setup.user_home();
    let file = claude_file(&user_home, "at-claude-code");
    let before = std::fs::read(&file).unwrap();
    let accounts = setup.accounts(Borrowed::discover(true, Some(&user_home)));
    let route = setup.route(&accounts, "anthropic").await.unwrap();
    caller_with(&accounts)
        .send(&route, &request("hi"), None)
        .await
        .unwrap();
    let refreshes = idp.received("/token");
    assert_eq!(refreshes.len(), 1);
    assert_eq!(
        refreshes[0].field("refresh_token").as_deref(),
        Some("rt-owned")
    );
    assert_eq!(
        std::fs::read(&file).unwrap(),
        before,
        "the borrowed login is untouched"
    );
    assert_eq!(bearers(&server).await, ["at-owned", "at-new"]);
}

#[tokio::test]
async fn a_borrowed_login_is_refreshed_after_a_401_and_written_back_in_place() {
    let idp = token_idp().await;
    let server = MockServer::start().await;
    ok_for(&server, "/responses", "at-new").await;
    otherwise_401(&server).await;
    let config = format!(
        "[providers.openai-codex]\nbase_url = \"{}\"\nkey_hosts = [\"127.0.0.1\"]\n\
         [providers.openai-codex.oauth]\ntoken_url = \"{}\"\nallowed_hosts = [\"127.0.0.1\"]\n",
        server.uri(),
        idp.url("/token")
    );
    let setup = Setup::new(&config, &[]);
    let codex_dir = setup.user_home().join(".codex");
    std::fs::create_dir_all(&codex_dir).unwrap();
    let file = codex_dir.join("auth.json");
    // An access token that already expired: it is still sent as-is, and
    // refreshed only once the provider refuses it.
    let login = json!({
        "OPENAI_API_KEY": null,
        "tokens": {"access_token": "at-old", "refresh_token": "rt-old", "account_id": "acct-1"},
        "last_refresh": "2020-01-01T00:00:00Z",
        "custom": "kept"
    });
    std::fs::write(&file, login.to_string()).unwrap();
    let accounts = setup.accounts(Borrowed::discover(true, Some(&setup.user_home())));
    let route = setup.route(&accounts, "openai-codex").await.unwrap();
    assert_eq!(
        route.credential.describe(),
        "subscription:openai-codex/borrowed:codex-cli"
    );
    caller_with(&accounts)
        .send(&route, &request("hi"), None)
        .await
        .unwrap();
    assert_eq!(bearers(&server).await, ["at-old", "at-new"]);
    let refreshes = idp.received("/token");
    assert_eq!(refreshes.len(), 1);
    assert_eq!(
        refreshes[0].field("refresh_token").as_deref(),
        Some("rt-old")
    );
    // Only the original file changed, and only its token fields.
    let entries: Vec<String> = std::fs::read_dir(&codex_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(entries, ["auth.json"]);
    let written: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(written["tokens"]["access_token"], "at-new");
    assert_eq!(written["tokens"]["refresh_token"], "rt-new");
    assert_eq!(written["tokens"]["account_id"], "acct-1");
    assert_eq!(written["custom"], "kept");
    assert_ne!(written["last_refresh"], "2020-01-01T00:00:00Z");
    assert!(setup.credentials.store().names().unwrap().is_empty());
}

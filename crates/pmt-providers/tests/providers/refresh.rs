//! Sign-in token refresh and dead-token quarantine (spec: model-providers,
//! Subscription tokens live only in the secrets store; Token refresh and
//! dead-token quarantine).

use std::process::{Command, Stdio};
use std::time::Duration;

use jiff::SignedDuration;
use pmt_providers::auth::Status;
use pmt_providers::auth::borrowed::Borrowed;
use pmt_providers::pool::PoolState;
use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::accounts::{Setup, bearers, caller_with};
use crate::idp::{Action, Idp};
use crate::support::{request, sse};

fn codex_config(token_url: &str, inference: &str) -> String {
    format!(
        "[providers.openai-codex]\nbase_url = \"{inference}\"\nkey_hosts = [\"127.0.0.1\"]\n\
         [providers.openai-codex.oauth]\ntoken_url = \"{token_url}\"\nallowed_hosts = [\"127.0.0.1\"]\n"
    )
}

async fn inference_ok_for(server: &MockServer, token: &str) {
    Mock::given(method("POST"))
        .and(path("/responses"))
        .and(header("authorization", format!("Bearer {token}").as_str()))
        .respond_with(sse("responses_tool_call.sse"))
        .mount(server)
        .await;
}

async fn inference_401(server: &MockServer) {
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_json(json!({"error": {"message": "token expired"}})),
        )
        .with_priority(10)
        .mount(server)
        .await;
}

/// Answers one refresh; any further use of the same token is refused.
async fn rotating_idp() -> Idp {
    Idp::start(|req, seen| match (req.path.as_str(), seen) {
        ("/token", 0) => Action::Reply(
            200,
            json!({"access_token": "at-2", "refresh_token": "rt-2", "expires_in": 3600}),
        ),
        ("/token", _) => Action::Reply(400, json!({"error": "refresh_token_reused"})),
        _ => Action::Reply(404, json!({})),
    })
    .await
}

#[tokio::test]
async fn an_expiring_token_is_refreshed_before_the_request() {
    let idp = rotating_idp().await;
    let server = MockServer::start().await;
    inference_ok_for(&server, "at-2").await;
    let setup = Setup::new(&codex_config(&idp.url("/token"), &server.uri()), &[]);
    setup.sign_in(
        "openai-codex",
        "default",
        "at-1",
        "rt-1",
        SignedDuration::from_secs(30),
    );
    let accounts = setup.accounts(Borrowed::none());
    let route = setup.route(&accounts, "openai-codex").await.unwrap();
    caller_with(&accounts)
        .send(&route, &request("hi"), None)
        .await
        .unwrap();
    let refreshes = idp.received("/token");
    assert_eq!(refreshes.len(), 1);
    assert_eq!(
        refreshes[0].field("grant_type").as_deref(),
        Some("refresh_token")
    );
    assert_eq!(refreshes[0].field("refresh_token").as_deref(), Some("rt-1"));
    assert_eq!(bearers(&server).await, ["at-2"]);
    let sub = setup
        .credentials
        .subscription("openai-codex", "default")
        .unwrap()
        .unwrap();
    assert_eq!(
        setup
            .credentials
            .refresh_token(&sub)
            .unwrap()
            .unwrap()
            .expose(),
        "rt-2"
    );
    // Tokens live only in the secrets store: no other file of the home
    // (config, state.db, logs, locks) holds them.
    let plain = setup.plain_files();
    for token in ["at-1", "at-2", "rt-1", "rt-2"] {
        assert!(
            !plain.contains(token),
            "{token} leaked outside the secrets store"
        );
    }
}

#[tokio::test]
async fn a_401_refreshes_only_the_refused_credential_and_retries() {
    let idp = rotating_idp().await;
    let server = MockServer::start().await;
    inference_ok_for(&server, "at-2").await;
    inference_401(&server).await;
    let setup = Setup::new(&codex_config(&idp.url("/token"), &server.uri()), &[]);
    setup.sign_in(
        "openai-codex",
        "default",
        "at-1",
        "rt-1",
        SignedDuration::from_hours(5),
    );
    let accounts = setup.accounts(Borrowed::none());
    let route = setup.route(&accounts, "openai-codex").await.unwrap();
    caller_with(&accounts)
        .send(&route, &request("hi"), None)
        .await
        .unwrap();
    assert_eq!(bearers(&server).await, ["at-1", "at-2"]);
    assert_eq!(idp.received("/token").len(), 1);
}

#[tokio::test]
async fn a_revoked_refresh_token_is_quarantined_until_a_new_sign_in() {
    let idp = Idp::start(|_, _| {
        Action::Reply(
            400,
            json!({"error": "invalid_grant", "error_description": "revoked"}),
        )
    })
    .await;
    let server = MockServer::start().await;
    inference_ok_for(&server, "at-new").await;
    inference_401(&server).await;
    let setup = Setup::new(&codex_config(&idp.url("/token"), &server.uri()), &[]);
    setup.sign_in(
        "openai-codex",
        "personal",
        "at-1",
        "rt-1",
        SignedDuration::from_hours(5),
    );
    let accounts = setup.accounts(Borrowed::none());
    let route = setup.route(&accounts, "openai-codex").await.unwrap();
    let caller = caller_with(&accounts);
    let message = "Sign-in for openai-codex (personal) expired or was revoked. Run 'prometheus auth add openai-codex' to sign in again.";
    let first = caller.send(&route, &request("hi"), None).await.unwrap_err();
    assert_eq!(
        first.to_route_error(&route).code().code,
        "auth.reauth_required"
    );
    assert_eq!(first.to_route_error(&route).message(), message);
    for _ in 0..3 {
        let err = caller
            .send(&route, &request("again"), None)
            .await
            .unwrap_err();
        let err = err.to_route_error(&route);
        assert_eq!(err.code().code, "auth.reauth_required");
        assert_eq!(err.message(), message);
        assert_eq!(err.exit_code(), 78);
    }
    assert_eq!(
        idp.received("/token").len(),
        1,
        "the dead token is never presented again"
    );
    assert_eq!(
        bearers(&server).await,
        ["at-1"],
        "refused requests are not sent"
    );
    let sub = setup
        .credentials
        .subscription("openai-codex", "personal")
        .unwrap()
        .unwrap();
    assert_eq!(sub.status, Status::Dead);
    assert!(setup.credentials.refresh_token(&sub).unwrap().is_none());
    // New resolutions name the quarantine.
    let err = setup.route(&accounts, "openai-codex").await.unwrap_err();
    assert_eq!(err.code().code, "auth.reauth_required");
    // A new sign-in with the same label clears it.
    setup.sign_in(
        "openai-codex",
        "personal",
        "at-new",
        "rt-new",
        SignedDuration::from_hours(5),
    );
    let sub = setup
        .credentials
        .subscription("openai-codex", "personal")
        .unwrap()
        .unwrap();
    assert_eq!(sub.status, Status::Ok);
    let route = setup.route(&accounts, "openai-codex").await.unwrap();
    caller.send(&route, &request("back"), None).await.unwrap();
    assert_eq!(bearers(&server).await.last().unwrap(), "at-new");
}

#[tokio::test]
async fn a_rate_limited_token_endpoint_benches_instead_of_killing() {
    let idp = Idp::start(|_, _| Action::Reply(429, json!({"error": "slow_down"}))).await;
    let server = MockServer::start().await;
    let setup = Setup::new(&codex_config(&idp.url("/token"), &server.uri()), &[]);
    setup.sign_in(
        "openai-codex",
        "default",
        "at-1",
        "rt-1",
        SignedDuration::from_secs(10),
    );
    let accounts = setup.accounts(Borrowed::none());
    let route = setup.route(&accounts, "openai-codex").await.unwrap();
    let err = caller_with(&accounts)
        .send(&route, &request("hi"), None)
        .await
        .unwrap_err();
    assert_eq!(
        err.to_error("openai-codex").code().code,
        "auth.credentials_benched"
    );
    let sub = setup
        .credentials
        .subscription("openai-codex", "default")
        .unwrap()
        .unwrap();
    assert_eq!(sub.status, Status::Ok);
    assert!(setup.credentials.refresh_token(&sub).unwrap().is_some());
    let states = setup.store.states("openai-codex").unwrap();
    assert!(states[0].benched_until.is_some(), "{states:?}");
    assert!(bearers(&server).await.is_empty());
}

// --- Two processes, one single-use refresh token. ---

const CHILD: &str = "PMT_TEST_REFRESH_CHILD";

/// Runs only as a child of `concurrent_refresh_uses_the_token_once`: signs
/// nothing, waits for the go file, then sends one request.
#[tokio::test]
async fn refresh_child() {
    let Ok(spec) = std::env::var(CHILD) else {
        return;
    };
    let spec: serde_json::Value = serde_json::from_str(&spec).unwrap();
    let field = |k: &str| spec[k].as_str().unwrap().to_owned();
    let config = codex_config(&field("token_url"), &field("inference"));
    let setup = Setup::reattach(std::path::Path::new(&field("dir")), &config);
    let accounts = setup.accounts(Borrowed::none());
    let route = setup.route(&accounts, "openai-codex").await.unwrap();
    let go = std::path::PathBuf::from(field("go"));
    while !go.exists() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    caller_with(&accounts)
        .send(&route, &request("hi"), None)
        .await
        .unwrap();
    println!("CHILD-OK");
}

#[tokio::test]
async fn concurrent_refresh_uses_the_token_once() {
    let token = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(
                    json!({"access_token": "at-2", "refresh_token": "rt-2", "expires_in": 3600}),
                )
                .set_delay(Duration::from_millis(300)),
        )
        .up_to_n_times(1)
        .mount(&token)
        .await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(400).set_body_json(json!({"error": "refresh_token_reused"})),
        )
        .mount(&token)
        .await;
    let server = MockServer::start().await;
    inference_ok_for(&server, "at-2").await;
    let token_url = format!("{}/token", token.uri());
    let setup = Setup::new(&codex_config(&token_url, &server.uri()), &[]);
    setup.sign_in(
        "openai-codex",
        "default",
        "at-1",
        "rt-1",
        SignedDuration::from_secs(5),
    );
    let go = setup.root.join("go");
    let spec = json!({
        "dir": setup.root,
        "token_url": token_url,
        "inference": server.uri(),
        "go": go,
    })
    .to_string();
    let children: Vec<_> = (0..2)
        .map(|_| {
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "refresh::refresh_child", "--nocapture"])
                .env(CHILD, &spec)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    // Both children resolve their route (expiring token) before the go.
    tokio::time::sleep(Duration::from_millis(300)).await;
    std::fs::write(&go, "").unwrap();
    for child in children {
        let out = tokio::task::spawn_blocking(move || child.wait_with_output().unwrap())
            .await
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success() && stdout.contains("CHILD-OK"),
            "{stdout}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let refreshes = token.received_requests().await.unwrap();
    assert_eq!(refreshes.len(), 1, "exactly one refresh request");
    assert_eq!(
        bearers(&server).await,
        ["at-2", "at-2"],
        "both used the rotated token"
    );
}

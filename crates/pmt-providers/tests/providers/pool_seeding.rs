//! Pool seeding, priority order, child sharing and resets through real
//! requests (spec: model-providers, Credential pool seeding, custom
//! endpoint pools and child sharing; Credential pool maintenance
//! commands).

use jiff::{SignedDuration, Timestamp};
use pmt_providers::auth::borrowed::Borrowed;
use pmt_providers::pool::{self, PoolState, secret_id};
use serde_json::json;
use wiremock::matchers::{header, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::accounts::{Setup, bearers, caller_with};
use crate::support::{request, sse};

async fn ok_for(server: &MockServer, key: &str) {
    Mock::given(method("POST"))
        .and(header("authorization", format!("Bearer {key}").as_str()))
        .respond_with(sse("chat_text.sse"))
        .mount(server)
        .await;
}

fn openrouter(server: &MockServer, strategy: &str) -> String {
    format!(
        "[providers.openrouter]\nbase_url = \"{}/v1\"\nkey_hosts = [\"127.0.0.1\"]\npool_strategy = \"{strategy}\"\n",
        server.uri()
    )
}

const SIBLINGS: &[(&str, &str)] = &[
    ("OPENROUTER_API_KEY", "key-a"),
    ("OPENROUTER_API_KEY_2", "key-b"),
];

#[tokio::test]
async fn numbered_siblings_and_custom_endpoint_keys_rotate_in_one_pool() {
    let server = MockServer::start().await;
    for key in ["key-a", "key-b", "own-1", "own-2"] {
        ok_for(&server, key).await;
    }
    let config = format!(
        "{}[providers.mine]\nbase_url = \"{}/mine/v1\"\nkey_secret = \"MINE_KEY\"\npool_strategy = \"round_robin\"\n",
        openrouter(&server, "round_robin"),
        server.uri()
    );
    let mut keys = SIBLINGS.to_vec();
    keys.extend([("MINE_KEY", "own-1"), ("MINE_KEY_2", "own-2")]);
    let setup = Setup::new(&config, &keys);
    let accounts = setup.accounts(Borrowed::none());
    let caller = caller_with(&accounts);
    for provider in ["openrouter", "openrouter", "mine", "mine"] {
        let route = setup.route(&accounts, provider).await.unwrap();
        caller.send(&route, &request("hi"), None).await.unwrap();
    }
    assert_eq!(bearers(&server).await, ["key-a", "key-b", "own-1", "own-2"]);
}

/// Scenario: Child sees parent's bench.
#[tokio::test]
async fn a_delegated_child_skips_the_member_its_parent_benched() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(header("authorization", "Bearer key-a"))
        .respond_with(
            ResponseTemplate::new(429).set_body_json(json!({"error": {"message": "slow down"}})),
        )
        .mount(&server)
        .await;
    ok_for(&server, "key-b").await;
    let setup = Setup::new(&openrouter(&server, "fill_first"), SIBLINGS);
    let parent = setup.accounts(Borrowed::none());
    let route = setup.route(&parent, "openrouter").await.unwrap();
    caller_with(&parent)
        .send(&route, &request("parent"), None)
        .await
        .unwrap();
    assert_eq!(bearers(&server).await, ["key-a", "key-b"]);

    // The child resolves and calls with its own accounts on the profile's
    // state.db.
    let child = setup.accounts(Borrowed::none());
    let route = setup.route(&child, "openrouter").await.unwrap();
    caller_with(&child)
        .send(&route, &request("child"), None)
        .await
        .unwrap();
    assert_eq!(bearers(&server).await, ["key-a", "key-b", "key-b"]);
}

/// Scenario: Reset reaches a running session.
#[tokio::test]
async fn a_reset_in_state_db_reaches_a_running_session_on_its_next_request() {
    let server = MockServer::start().await;
    ok_for(&server, "key-a").await;
    let setup = Setup::new(&openrouter(&server, "fill_first"), &SIBLINGS[..1]);
    let running = setup.accounts(Borrowed::none());
    let route = setup.route(&running, "openrouter").await.unwrap();
    let id = secret_id("OPENROUTER_API_KEY");
    let hour = Timestamp::now() + SignedDuration::from_hours(1);
    setup
        .store
        .bench("openrouter", &id, Some(hour), Some("billing"))
        .unwrap();
    let caller = caller_with(&running);
    let err = caller.send(&route, &request("hi"), None).await.unwrap_err();
    assert_eq!(
        err.to_error("openrouter").code().code,
        "auth.credentials_benched"
    );

    // `prometheus auth reset openrouter` from another process.
    setup
        .reopen_store()
        .bench("openrouter", &id, None, None)
        .unwrap();
    caller.send(&route, &request("again"), None).await.unwrap();
    assert_eq!(bearers(&server).await, ["key-a"]);
}

#[tokio::test]
async fn the_priority_order_decides_which_key_fill_first_takes() {
    let server = MockServer::start().await;
    ok_for(&server, "key-a").await;
    ok_for(&server, "key-b").await;
    let setup = Setup::new(&openrouter(&server, "fill_first"), SIBLINGS);
    let accounts = setup.accounts(Borrowed::none());
    pool::set_priority(
        setup.store.as_ref(),
        "openrouter",
        &[secret_id("OPENROUTER_API_KEY_2")],
    )
    .unwrap();
    assert_eq!(
        pool::priority(Some(setup.store.as_ref() as &dyn PoolState), "openrouter"),
        [secret_id("OPENROUTER_API_KEY_2")]
    );
    let route = setup.route(&accounts, "openrouter").await.unwrap();
    caller_with(&accounts)
        .send(&route, &request("hi"), None)
        .await
        .unwrap();
    assert_eq!(bearers(&server).await, ["key-b"]);
}

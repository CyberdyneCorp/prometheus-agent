//! Credential pools and benches through real requests (spec:
//! model-providers, Credential pool selection strategies; Exhausted
//! credential cooldowns).

use jiff::{SignedDuration, Timestamp};
use pmt_providers::auth::borrowed::Borrowed;
use pmt_providers::pool::{PoolState, secret_id};
use serde_json::json;
use wiremock::matchers::{header, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::accounts::{Setup, bearers, caller_with};
use crate::support::{request, sse};

fn config(server: &MockServer, strategy: &str) -> String {
    format!(
        "[providers.openrouter]\nbase_url = \"{}/v1\"\nkey_hosts = [\"127.0.0.1\"]\npool_strategy = \"{strategy}\"\n",
        server.uri()
    )
}

const KEYS: &[(&str, &str)] = &[
    ("OPENROUTER_API_KEY", "key-a"),
    ("OPENROUTER_API_KEY_B", "key-b"),
];

async fn ok_for(server: &MockServer, key: &str) {
    Mock::given(method("POST"))
        .and(header("authorization", format!("Bearer {key}").as_str()))
        .respond_with(sse("chat_text.sse"))
        .mount(server)
        .await;
}

fn count(setup: &Setup, id: &str) -> u64 {
    setup
        .store
        .states("openrouter")
        .unwrap()
        .into_iter()
        .find(|s| s.credential == id)
        .map_or(0, |s| s.request_count)
}

#[tokio::test]
async fn round_robin_rotates_and_counts_requests() {
    let server = MockServer::start().await;
    ok_for(&server, "key-a").await;
    ok_for(&server, "key-b").await;
    let setup = Setup::new(&config(&server, "round_robin"), KEYS);
    let accounts = setup.accounts(Borrowed::none());
    let route = setup.route(&accounts, "openrouter").await.unwrap();
    assert!(route.credential.describe().starts_with("pool:openrouter#"));
    let caller = caller_with(&accounts);
    for _ in 0..4 {
        caller.send(&route, &request("hi"), None).await.unwrap();
    }
    assert_eq!(bearers(&server).await, ["key-a", "key-b", "key-a", "key-b"]);
    assert_eq!(count(&setup, &secret_id("OPENROUTER_API_KEY")), 2);
    assert_eq!(count(&setup, &secret_id("OPENROUTER_API_KEY_B")), 2);
}

#[tokio::test]
async fn a_429d_key_is_skipped_until_its_cooldown_ends() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(header("authorization", "Bearer key-a"))
        .respond_with(
            ResponseTemplate::new(429).set_body_json(json!({"error": {"message": "rate limited"}})),
        )
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    ok_for(&server, "key-a").await;
    ok_for(&server, "key-b").await;
    let setup = Setup::new(&config(&server, "fill_first"), KEYS);
    let accounts = setup.accounts(Borrowed::none());
    let route = setup.route(&accounts, "openrouter").await.unwrap();
    let caller = caller_with(&accounts);
    // The 429 benches key A; the same-route retry takes key B.
    caller.send(&route, &request("one"), None).await.unwrap();
    caller.send(&route, &request("two"), None).await.unwrap();
    assert_eq!(bearers(&server).await, ["key-a", "key-b", "key-b"]);
    let a = secret_id("OPENROUTER_API_KEY");
    let bench = setup
        .store
        .states("openrouter")
        .unwrap()
        .into_iter()
        .find(|s| s.credential == a)
        .and_then(|s| s.benched_until)
        .unwrap();
    let left = bench.duration_since(Timestamp::now()).as_secs();
    assert!(
        (3590..=3600).contains(&left),
        "two keys: a full hour ({left} s)"
    );
    // The bench is in state.db: a new process (a reopened store) sees it.
    let reopened = setup.reopen_store();
    let states = PoolState::states(&reopened, "openrouter").unwrap();
    assert!(
        states
            .iter()
            .any(|s| s.credential == a && s.benched_until == Some(bench))
    );
    let route = setup.route(&accounts, "openrouter").await.unwrap();
    assert_eq!(route.credential.describe(), "pool:openrouter#2");
    // Once the cooldown is over, key A serves again.
    setup
        .store
        .bench(
            "openrouter",
            &a,
            Some(Timestamp::now() - SignedDuration::from_secs(1)),
            None,
        )
        .unwrap();
    caller.send(&route, &request("three"), None).await.unwrap();
    assert_eq!(bearers(&server).await.last().unwrap(), "key-a");
}

#[tokio::test]
async fn a_sole_throttled_key_is_benched_a_minute_and_refuses_requests_meanwhile() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(429).set_body_json(json!({"error": {"message": "slow"}})),
        )
        .mount(&server)
        .await;
    let setup = Setup::new(&config(&server, "fill_first"), &KEYS[..1]);
    let accounts = setup.accounts(Borrowed::none());
    let route = setup.route(&accounts, "openrouter").await.unwrap();
    assert_eq!(route.credential.describe(), "secret:OPENROUTER_API_KEY");
    let err = caller_with(&accounts)
        .send(&route, &request("hi"), None)
        .await
        .unwrap_err();
    assert_eq!(
        err.to_error("openrouter").code().code,
        "auth.credentials_benched"
    );
    assert_eq!(
        bearers(&server).await.len(),
        1,
        "the benched key is not sent again"
    );
    let bench = setup.store.states("openrouter").unwrap()[0]
        .benched_until
        .unwrap();
    let left = bench.duration_since(Timestamp::now()).as_secs();
    assert!(
        (50..=60).contains(&left),
        "sole key: at most a minute ({left} s)"
    );
    // Resolution names the bench too.
    let err = setup.route(&accounts, "openrouter").await.unwrap_err();
    assert_eq!(err.code().code, "auth.credentials_benched");
    assert_eq!(err.exit_code(), 69);
}

#[tokio::test]
async fn a_billing_failure_keeps_the_full_bench_even_for_a_sole_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(402)
                .set_body_json(json!({"error": {"message": "payment required"}})),
        )
        .mount(&server)
        .await;
    let setup = Setup::new(&config(&server, "fill_first"), &KEYS[..1]);
    let accounts = setup.accounts(Borrowed::none());
    let route = setup.route(&accounts, "openrouter").await.unwrap();
    caller_with(&accounts)
        .send(&route, &request("hi"), None)
        .await
        .unwrap_err();
    let bench = setup.store.states("openrouter").unwrap()[0]
        .benched_until
        .unwrap();
    let left = bench.duration_since(Timestamp::now()).as_secs();
    assert!((3590..=3600).contains(&left), "{left} s");
}

#[tokio::test]
async fn auxiliary_calls_draw_from_the_same_pool() {
    use pmt_providers::auxiliary::{AuxCaller, AuxRoute, AuxSource, AuxTask};
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(header("authorization", "Bearer key-a"))
        .respond_with(
            ResponseTemplate::new(429).set_body_json(json!({"error": {"message": "rate limited"}})),
        )
        .mount(&server)
        .await;
    ok_for(&server, "key-b").await;
    let setup = Setup::new(&config(&server, "fill_first"), KEYS);
    let accounts = setup.accounts(Borrowed::none());
    let aux = AuxRoute {
        task: AuxTask::Compression,
        route: setup.route(&accounts, "openrouter").await.unwrap(),
        source: AuxSource::Config,
        timeout: std::time::Duration::from_secs(10),
        retries: 2,
    };
    let ctx = pmt_providers::cost::CallContext::new(pmt_core::id::SessionId::new(), "main");
    AuxCaller::new(crate::accounts::http())
        .with_credentials(accounts.clone())
        .call(&aux, &request("summarise"), &ctx)
        .await
        .unwrap();
    assert_eq!(bearers(&server).await, ["key-a", "key-b"]);
    let benched = setup.store.states("openrouter").unwrap();
    assert!(
        benched
            .iter()
            .any(|s| s.credential == secret_id("OPENROUTER_API_KEY") && s.benched_until.is_some())
    );
}

/// Regression: two requests in the same millisecond both picked key A
/// (`last_used_at` tied); the stored use sequence keeps A, B, A, B.
#[test]
fn round_robin_rotates_through_state_db_within_one_millisecond() {
    use pmt_config::schema::PoolStrategy;
    use pmt_providers::pool::select;

    let setup = Setup::new("", KEYS);
    let ids = [
        secret_id("OPENROUTER_API_KEY"),
        secret_id("OPENROUTER_API_KEY_B"),
    ];
    let now = Timestamp::now();
    let reopened = setup.reopen_store();
    let used: Vec<usize> = (0..4)
        .map(|i| {
            // Alternate handles: two processes sharing the profile.
            let state: &dyn PoolState = if i % 2 == 0 { &*setup.store } else { &reopened };
            let pick = select(
                PoolStrategy::RoundRobin,
                &ids,
                &state.states("openrouter").unwrap(),
                now,
            )
            .unwrap();
            state.record_use("openrouter", &ids[pick], now).unwrap();
            pick
        })
        .collect();
    assert_eq!(used, [0, 1, 0, 1]);
}

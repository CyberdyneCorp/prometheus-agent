//! Routing upgrades (spec: model-providers, Subscription plan and quota
//! visibility; Billing source transparency; Scoped fallback chains for
//! pinned owners; Provider health scoring; Provider spend caps).

use std::sync::Arc;
use std::time::Duration;

use jiff::{SignedDuration, Timestamp};
use pmt_obs::usage::{Call, ConfigPrices, UsageRecorder};
use pmt_providers::auth::Accounts;
use pmt_providers::auth::borrowed::Borrowed;
use pmt_providers::capability::Needs;
use pmt_providers::classify::ErrorClass;
use pmt_providers::fallback::{self, Fallback, Healthy, MeteredConfirm, RouteHealth, Walk};
use pmt_providers::health::StoreHealth;
use pmt_providers::quota::QuotaTracker;
use pmt_providers::resolve::{BoxFuture, Resolver, TcpProbe};
use pmt_providers::spend::SpendCaps;
use pmt_providers::{CallError, ModelCaller, Route};
use pmt_store::provider_state::CallSample;
use pmt_store::{NewSession, Source, Tokens};
use serde_json::json;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::accounts::{Setup, http};
use crate::support::{caller, request, sse};

static PROBE: TcpProbe = TcpProbe {
    timeout: Duration::from_millis(200),
};

fn usage_body(used: f64, reset: Timestamp) -> serde_json::Value {
    json!({
        "plan_type": "plus",
        "rate_limit": {
            "primary_window": {"used_percent": used, "limit_window_seconds": 18_000, "reset_at": reset.as_second()},
            "secondary_window": {"used_percent": 10.0, "limit_window_seconds": 604_800}
        }
    })
}

/// A ChatGPT subscription on `server` and, optionally, an `openrouter`
/// key and fallback entry there too.
fn codex_setup(server: &MockServer, extra: &str, chain: bool) -> Setup {
    let mut config = format!(
        "[providers.openai-codex]\nbase_url = \"{uri}/backend-api/codex\"\nkey_hosts = [\"127.0.0.1\"]\nmax_retries = 0\n\
         [providers.openrouter]\nbase_url = \"{uri}/or/v1\"\nkey_hosts = [\"127.0.0.1\"]\nmax_retries = 0\n{extra}",
        uri = server.uri()
    );
    if chain {
        config.push_str("[[fallback.chain]]\nprovider = \"openrouter\"\nmodel = \"m\"\n");
    }
    let setup = Setup::new(
        &config,
        &[("OPENROUTER_API_KEY", "sk-or-v1-routing-0123456789")],
    );
    setup.sign_in(
        "openai-codex",
        "personal",
        "at-personal",
        "rt-personal",
        SignedDuration::from_hours(1),
    );
    setup
}

fn quota_accounts(setup: &Setup, refresh: Duration) -> Arc<Accounts> {
    let accounts = Arc::try_unwrap(setup.accounts(Borrowed::none())).unwrap();
    let tracker = QuotaTracker::new(http(), Arc::clone(&setup.store)).with_refresh(refresh);
    Arc::new(accounts.with_quota(tracker))
}

async fn mount_usage(server: &MockServer, used: f64, reset: Timestamp) {
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/backend-api/wham/usage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(usage_body(used, reset)))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/responses"))
        .respond_with(sse("responses_tool_call.sse"))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path_regex("^/or/v1/"))
        .respond_with(sse("chat_text.sse"))
        .mount(server)
        .await;
}

async fn inference_posts(server: &MockServer, prefix: &str) -> usize {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| r.method.as_str() == "POST" && r.url.path().starts_with(prefix))
        .count()
}

fn resolver<'a>(setup: &'a Setup, accounts: &'a Accounts) -> Resolver<'a> {
    Resolver {
        registry: &setup.registry,
        config: &setup.config,
        secrets: &setup.keys,
        extras: accounts,
        probe: &PROBE,
        ad_hoc: None,
    }
}

async fn call_chain(
    setup: &Setup,
    accounts: &Arc<Accounts>,
    state: &mut Fallback,
    health: &dyn RouteHealth,
) -> Result<pmt_providers::Called, CallError> {
    let resolver = resolver(setup, accounts);
    let req = request("x");
    let walk = Walk {
        resolver: &resolver,
        health,
        catalog: &pmt_providers::metadata::Catalog::new(&setup.config, None),
        needs: Needs::of(&req),
    };
    caller()
        .with_credentials(Arc::clone(accounts) as _)
        .call(state, &walk, &req, None)
        .await
}

#[tokio::test]
async fn exhausted_quota_benched_until_reset() {
    let server = MockServer::start().await;
    let reset = Timestamp::now() + SignedDuration::from_mins(40);
    mount_usage(&server, 100.0, reset).await;
    let setup = codex_setup(&server, "", false);
    let accounts = quota_accounts(&setup, Duration::from_mins(1));
    let route = setup.route(&accounts, "openai-codex").await.unwrap();
    let refused = caller()
        .with_credentials(Arc::clone(&accounts) as _)
        .send(&route, &request("x"), None)
        .await
        .unwrap_err();
    let error = refused.to_error("openai-codex");
    assert_eq!(error.code().code, "auth.credentials_benched");
    let shown = reset.strftime("%Y-%m-%dT%H:%M:%SZ").to_string();
    assert!(error.message().contains(&shown), "{}", error.message());
    assert!(error.hint().unwrap().contains("5h 100%"));
    assert_eq!(inference_posts(&server, "/backend-api/codex").await, 0);
    let state = setup.store.credential_states("openai-codex").unwrap();
    assert_eq!(state[0].bench_reason.as_deref(), Some("quota"));
    // Within the minute the stored probe answers: still refused, unsent.
    caller()
        .with_credentials(Arc::clone(&accounts) as _)
        .send(&route, &request("x"), None)
        .await
        .unwrap_err();
    let probes = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/backend-api/wham/usage")
        .count();
    assert_eq!(probes, 1);
}

#[tokio::test]
async fn a_quota_restored_early_clears_the_bench() {
    let server = MockServer::start().await;
    let reset = Timestamp::now() + SignedDuration::from_mins(40);
    mount_usage(&server, 100.0, reset).await;
    let setup = codex_setup(&server, "", false);
    let accounts = quota_accounts(&setup, Duration::ZERO);
    let route = setup.route(&accounts, "openai-codex").await.unwrap();
    let caller = caller().with_credentials(Arc::clone(&accounts) as _);
    let req = request("x");
    caller.send(&route, &req, None).await.unwrap_err();
    mount_usage(&server, 20.0, reset).await;
    caller
        .send(&route, &req, None)
        .await
        .unwrap()
        .collect()
        .await;
    assert_eq!(inference_posts(&server, "/backend-api/codex").await, 1);
    let state = setup.store.credential_states("openai-codex").unwrap();
    assert_eq!(state[0].benched_until, None);
}

#[tokio::test]
async fn fallback_from_subscription_to_api_key_is_announced() {
    let server = MockServer::start().await;
    mount_usage(
        &server,
        100.0,
        Timestamp::now() + SignedDuration::from_mins(40),
    )
    .await;
    let setup = codex_setup(&server, "", true);
    let accounts = quota_accounts(&setup, Duration::from_mins(1));
    let primary = setup.route(&accounts, "openai-codex").await.unwrap();
    let chain = fallback::chain(&setup.config, &setup.registry).unwrap();
    let mut state = Fallback::new(primary, chain, 0.2);
    let called = call_chain(&setup, &accounts, &mut state, &Healthy)
        .await
        .unwrap();
    assert_eq!(called.route.provider, "openrouter");
    let event = called.fallbacks[0].payload();
    assert!(event.billing_change);
    assert_eq!(
        event.from_billing.as_deref(),
        Some("subscription:openai-codex/personal")
    );
    assert_eq!(event.to_billing.as_deref(), Some("api_key:openrouter"));
    assert_eq!(event.reason, "auth.credentials_benched");
    assert_eq!(
        fallback::billing_label("subscription:openai-codex/personal"),
        "subscription openai-codex/personal"
    );
    assert_eq!(
        fallback::billing_label("api_key:openrouter/default"),
        "API key openrouter/default"
    );
}

struct Answer(bool);

impl MeteredConfirm for Answer {
    fn confirm<'a>(&'a self, from: &'a str, to: &'a str) -> BoxFuture<'a, bool> {
        assert_eq!(from, "subscription openai-codex/personal");
        assert_eq!(to, "API key openrouter");
        Box::pin(async move { self.0 })
    }
}

#[tokio::test]
async fn a_gated_metered_fallback_needs_confirmation() {
    let server = MockServer::start().await;
    mount_usage(
        &server,
        100.0,
        Timestamp::now() + SignedDuration::from_mins(40),
    )
    .await;
    let text = format!(
        "[providers.openai-codex]\nbase_url = \"{uri}/backend-api/codex\"\nkey_hosts = [\"127.0.0.1\"]\nmax_retries = 0\nconfirm_metered_fallback = true\n\
         [providers.openrouter]\nbase_url = \"{uri}/or/v1\"\nkey_hosts = [\"127.0.0.1\"]\nmax_retries = 0\n\
         [[fallback.chain]]\nprovider = \"openrouter\"\nmodel = \"m\"\n",
        uri = server.uri()
    );
    let setup = Setup::new(
        &text,
        &[("OPENROUTER_API_KEY", "sk-or-v1-routing-0123456789")],
    );
    setup.sign_in(
        "openai-codex",
        "personal",
        "at-personal",
        "rt-personal",
        SignedDuration::from_hours(1),
    );
    let accounts = quota_accounts(&setup, Duration::from_mins(1));
    let primary = setup.route(&accounts, "openai-codex").await.unwrap();
    let chain = fallback::chain(&setup.config, &setup.registry).unwrap();
    // Non-interactive: no one to ask, so the turn fails.
    let mut state = Fallback::new(primary.clone(), chain.clone(), 0.2);
    let Err(CallError::Failed(failed)) = call_chain(&setup, &accounts, &mut state, &Healthy).await
    else {
        panic!("a metered move without confirmation must fail");
    };
    assert_eq!(
        failed.to_error().code().code,
        "provider.metered_fallback_refused"
    );
    assert_eq!(inference_posts(&server, "/or/v1").await, 0);
    // Declined, then confirmed.
    let mut state = Fallback::new(primary.clone(), chain.clone(), 0.2)
        .with_metered_confirm(Arc::new(Answer(false)));
    assert!(
        call_chain(&setup, &accounts, &mut state, &Healthy)
            .await
            .is_err()
    );
    let mut state = Fallback::new(primary, chain, 0.2).with_metered_confirm(Arc::new(Answer(true)));
    let called = call_chain(&setup, &accounts, &mut state, &Healthy)
        .await
        .unwrap();
    assert_eq!(called.route.provider, "openrouter");
}

#[tokio::test]
async fn monthly_cap_reached_falls_back() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path_regex("^/or/v1/"))
        .respond_with(sse("chat_text.sse"))
        .mount(&server)
        .await;
    let text = format!(
        "[providers.anthropic]\nbase_url = \"{uri}/anthropic\"\nkey_hosts = [\"127.0.0.1\"]\nmonthly_budget_usd = 50.0\nmax_retries = 0\n\
         [providers.openrouter]\nbase_url = \"{uri}/or/v1\"\nkey_hosts = [\"127.0.0.1\"]\nmax_retries = 0\n\
         [[fallback.chain]]\nprovider = \"openrouter\"\nmodel = \"m\"\n",
        uri = server.uri()
    );
    let setup = Setup::new(
        &text,
        &[
            ("ANTHROPIC_API_KEY", "sk-ant-api03-routing-0123456789"),
            ("OPENROUTER_API_KEY", "sk-or-v1-routing-0123456789"),
        ],
    );
    let session = setup
        .store
        .create_session(&NewSession::new(Source::Cli))
        .unwrap()
        .id;
    let recorder = UsageRecorder::new(Arc::clone(&setup.store), Arc::new(ConfigPrices::default()));
    for usd in [30.0, 20.10] {
        recorder
            .record_at(
                Call {
                    tokens: Tokens {
                        input: 1000,
                        ..Tokens::default()
                    },
                    reported_usd: Some(usd),
                    billing_source: "api_key:anthropic".to_owned(),
                    ..Call::new(session, "main", "anthropic", "claude-sonnet-4-5")
                },
                Timestamp::now(),
            )
            .unwrap();
    }
    let accounts = Arc::new(
        Arc::try_unwrap(setup.accounts(Borrowed::none()))
            .unwrap()
            .with_spend_caps(SpendCaps::new(Arc::clone(&setup.store))),
    );
    let primary = setup.route(&accounts, "anthropic").await.unwrap();
    let chain = fallback::chain(&setup.config, &setup.registry).unwrap();
    let mut state = Fallback::new(primary, chain, 0.2);
    let called = call_chain(&setup, &accounts, &mut state, &Healthy)
        .await
        .unwrap();
    assert_eq!(called.route.provider, "openrouter");
    let event = called.fallbacks[0].payload();
    assert_eq!(event.reason, "provider.budget_exhausted");
    assert_eq!(called.fallbacks[0].reason, ErrorClass::Billing);
    assert_eq!(inference_posts(&server, "/anthropic").await, 0);
}

#[tokio::test]
async fn the_eighty_percent_warning_is_emitted_once_per_period() {
    let setup = Setup::new("[providers.anthropic]\ndaily_budget_usd = 10.0\n", &[]);
    let session = setup
        .store
        .create_session(&NewSession::new(Source::Cli))
        .unwrap()
        .id;
    let recorder = UsageRecorder::new(Arc::clone(&setup.store), Arc::new(ConfigPrices::default()));
    recorder
        .record_at(
            Call {
                reported_usd: Some(8.5),
                billing_source: "api_key:anthropic".to_owned(),
                ..Call::new(session, "main", "anthropic", "m")
            },
            Timestamp::now(),
        )
        .unwrap();
    let caps = SpendCaps::new(Arc::clone(&setup.store));
    let provider = setup.registry.lookup("anthropic").unwrap();
    caps.check(provider, Timestamp::now()).unwrap();
    let today = format!(
        "daily:{}",
        Timestamp::now().to_zoned(jiff::tz::TimeZone::UTC).date()
    );
    assert!(
        !setup
            .store
            .mark_budget_warning("anthropic", &today, Timestamp::now())
            .unwrap(),
        "the check already warned for today"
    );
}

#[tokio::test]
async fn pinned_cron_job_gets_no_inherited_chain() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(529).set_body_string("overloaded"))
        .mount(&server)
        .await;
    let text = format!(
        "[providers.prim]\nbase_url = \"{uri}/prim/v1\"\nmax_retries = 0\n\
         [providers.b]\nbase_url = \"{uri}/b/v1\"\nmax_retries = 0\n\
         [[fallback.chain]]\nprovider = \"b\"\nmodel = \"m\"\n",
        uri = server.uri()
    );
    let setup = Setup::new(&text, &[]);
    let profile_chain = fallback::chain(&setup.config, &setup.registry).unwrap();
    let pinned =
        fallback::scoped_chain(true, None, profile_chain.clone(), &setup.registry).unwrap();
    assert!(pinned.is_empty());
    let inherited =
        fallback::scoped_chain(false, None, profile_chain.clone(), &setup.registry).unwrap();
    assert_eq!(inherited, profile_chain);
    let disabled =
        fallback::scoped_chain(false, Some(&[]), profile_chain.clone(), &setup.registry).unwrap();
    assert!(disabled.is_empty());
    let own = [pmt_config::schema::FallbackEntry {
        provider: "prim".to_owned(),
        model: "other".to_owned(),
        ..Default::default()
    }];
    let declared =
        fallback::scoped_chain(true, Some(&own), profile_chain, &setup.registry).unwrap();
    assert_eq!(declared[0].model.as_deref(), Some("other"));

    let accounts = setup.accounts(Borrowed::none());
    let primary = setup.route(&accounts, "prim").await.unwrap();
    let mut state = Fallback::new(primary, pinned, 0.2);
    let Err(CallError::Failed(failed)) = call_chain(&setup, &accounts, &mut state, &Healthy).await
    else {
        panic!("the pinned job's route must fail");
    };
    assert_eq!(failed.error.class, ErrorClass::Overloaded);
    assert!(failed.fallbacks.is_empty());
    assert_eq!(inference_posts(&server, "/b/").await, 0);
}

#[tokio::test]
async fn repeated_overloads_lower_the_score() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(529).set_body_string("overloaded"))
        .mount(&server)
        .await;
    let text = format!(
        "[providers.prim]\nbase_url = \"{}/prim/v1\"\nmax_retries = 0\n",
        server.uri()
    );
    let setup = Setup::new(&text, &[]);
    let health = Arc::new(StoreHealth::new(Arc::clone(&setup.store), &setup.config));
    let caller: ModelCaller = caller().with_observer(Arc::clone(&health) as _);
    let accounts = setup.accounts(Borrowed::none());
    let route: Route = setup.route(&accounts, "prim").await.unwrap();
    for _ in 0..10 {
        caller.send(&route, &request("x"), None).await.unwrap_err();
    }
    let records = health.records(Timestamp::now()).unwrap();
    assert_eq!(records.len(), 1);
    assert!(records[0].score < 0.2, "{records:?}");
    assert_eq!(records[0].errors["overloaded"], 10);
    assert!(health.score("prim", "m") < 0.2);
    assert!(health.score("prim", "unknown") > 0.99);
}

#[tokio::test]
async fn idle_provider_recovers() {
    let setup = Setup::new("", &[]);
    let hour_ago = Timestamp::now() - SignedDuration::from_secs(3600);
    let sample = |ts, ok: bool| CallSample {
        provider: "p".to_owned(),
        model: "m".to_owned(),
        credential: "secret:K".to_owned(),
        ts,
        class: (!ok).then(|| "overloaded".to_owned()),
        ttft_ms: ok.then_some(40_000),
        output_tps: None,
    };
    // One slow success in nine calls: score about 0.1.
    for i in 0..9 {
        setup
            .store
            .record_provider_call(&sample(hour_ago, i == 0))
            .unwrap();
    }
    let health = StoreHealth::new(Arc::clone(&setup.store), &setup.config);
    let then = health.records(hour_ago).unwrap()[0].score;
    assert!((0.09..=0.11).contains(&then), "{then}");
    let now = health.score("p", "m");
    assert!(now >= 0.7, "{now}");
}

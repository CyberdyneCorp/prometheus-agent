//! Session-start routing (spec: model-providers, Cost and latency aware
//! routing policy): candidates resolved against the profile's providers,
//! filtered by credentials, health and capability, then ranked.

use std::collections::BTreeMap;

use pmt_config::schema::RoutingPolicy;
use pmt_providers::fallback::RouteHealth;
use pmt_providers::resolve::Target;
use pmt_providers::routing::{Context, Selection, select};

use crate::support::Env;

/// Four OpenAI-compatible providers priced cheapest to dearest (`pd` has
/// no key).
const CONFIG: &str = "\
[providers.pa]\nbase_url = \"https://a.example/v1\"\nkey_secret = \"PA_KEY\"\n\
[providers.pb]\nbase_url = \"https://b.example/v1\"\nkey_secret = \"PB_KEY\"\n\
[providers.pc]\nbase_url = \"https://c.example/v1\"\nkey_secret = \"PC_KEY\"\n\
[providers.pd]\nbase_url = \"https://d.example/v1\"\nkey_secret = \"PD_KEY\"\n\
[models.\"pa/m\".price]\ninput = 0.1\noutput = 0.4\n\
[models.\"pb/m\".price]\ninput = 1.0\noutput = 4.0\n\
[models.\"pc/m\".price]\ninput = 5.0\noutput = 20.0\n\
[models.\"pd/m\".price]\ninput = 0.01\noutput = 0.01\n";

const KEYS: &[(&str, &str)] = &[
    ("PA_KEY", "pa-key-0123456789"),
    ("PB_KEY", "pb-key-0123456789"),
    ("PC_KEY", "pc-key-0123456789"),
];

/// Health and latency by provider.
struct Scores(BTreeMap<&'static str, (f64, Option<u64>)>);

impl RouteHealth for Scores {
    fn score(&self, provider: &str, _model: &str) -> f64 {
        self.0.get(provider).map_or(1.0, |s| s.0)
    }

    fn ttft_p50_ms(&self, provider: &str, _model: &str) -> Option<u64> {
        self.0.get(provider).and_then(|s| s.1)
    }
}

fn target(provider: &str) -> Target {
    Target {
        provider: provider.to_owned(),
        model: Some("m".to_owned()),
        ..Target::default()
    }
}

async fn route(env: &Env, policy: RoutingPolicy, health: &Scores, names: &[&str]) -> Selection {
    let resolver = env.resolver();
    let ctx = Context {
        resolver: &resolver,
        health,
        catalog: &env.catalog,
        min_health: 0.2,
    };
    let candidates: Vec<Target> = names.iter().map(|n| target(n)).collect();
    select(policy, 0.5, &candidates, &ctx).await.unwrap()
}

/// Scenario: Cheapest healthy capable candidate wins.
#[tokio::test]
async fn cheapest_healthy_capable_candidate_wins() {
    let env = Env::new(CONFIG, KEYS);
    let health = Scores(BTreeMap::from([
        ("pa", (0.1, Some(500))),
        ("pb", (0.9, Some(900))),
        ("pc", (1.0, Some(300))),
    ]));
    let chosen = route(
        &env,
        RoutingPolicy::Cheapest,
        &health,
        &["pa", "pb", "pc", "pd"],
    )
    .await;
    assert_eq!(chosen.route.label(), "pb/m");
    assert_eq!(chosen.target, target("pb"));
    let event = chosen.event;
    assert_eq!(event.policy, "cheapest");
    assert_eq!(event.selected, "pb/m");
    let rejected: Vec<(&str, &str)> = event
        .rejected
        .iter()
        .map(|r| (r.route.as_str(), r.reason.as_str()))
        .collect();
    assert_eq!(
        rejected,
        [
            ("pa/m", "health"),
            ("pc/m", "cost"),
            ("pd/m", "credentials")
        ]
    );
    // The inputs of the decision: a typical turn's cost and the latency.
    let b = &event.candidates[0];
    assert_eq!(b.route, "pb/m");
    assert!((b.cost_usd.unwrap() - 0.012).abs() < 1e-9, "{b:?}");
    assert_eq!(b.ttft_p50_ms, Some(900));
}

#[tokio::test]
async fn fastest_and_balanced_rank_the_eligible_candidates() {
    let env = Env::new(CONFIG, KEYS);
    let health = Scores(BTreeMap::from([
        ("pa", (1.0, Some(2_000))),
        ("pb", (1.0, Some(1_000))),
        ("pc", (1.0, Some(300))),
    ]));
    let fastest = route(&env, RoutingPolicy::Fastest, &health, &["pa", "pb", "pc"]).await;
    assert_eq!(fastest.event.selected, "pc/m");
    // Balanced: pa (cost 0, latency 1), pb (cost ~0.15, latency ~0.41),
    // pc (cost 1, latency 0).
    let balanced = route(&env, RoutingPolicy::Balanced, &health, &["pa", "pb", "pc"]).await;
    assert_eq!(balanced.event.selected, "pb/m");
    assert!(balanced.event.rejected.iter().all(|r| r.reason == "score"));
}

#[tokio::test]
async fn pinned_or_nothing_eligible_chooses_nothing() {
    let env = Env::new(CONFIG, KEYS);
    let health = Scores(BTreeMap::from([("pa", (0.0, None))]));
    let resolver = env.resolver();
    let ctx = Context {
        resolver: &resolver,
        health: &health,
        catalog: &env.catalog,
        min_health: 0.2,
    };
    let none = select(
        RoutingPolicy::Cheapest,
        0.5,
        &[target("pa"), target("pd")],
        &ctx,
    )
    .await;
    assert!(none.is_none());
    let pinned = select(RoutingPolicy::Pinned, 0.5, &[target("pb")], &ctx).await;
    assert!(pinned.is_none());
}

//! Pool selection and bench rules (spec: model-providers, Credential pool
//! selection strategies; Exhausted credential cooldowns).

use std::time::Duration;

use jiff::{SignedDuration, Timestamp};
use pmt_config::Config;
use pmt_config::schema::PoolStrategy;

use super::*;
use crate::ProviderRegistry;

fn ids(names: &[&str]) -> Vec<String> {
    names.iter().map(|n| secret_id(n)).collect()
}

/// Serves `n` requests from `ids` by `strategy`, recording each use.
fn serve(strategy: PoolStrategy, ids: &[String], state: &MemoryState, n: usize) -> Vec<usize> {
    let mut now = Timestamp::from_second(1_800_000_000).unwrap();
    (0..n)
        .map(|_| {
            now += SignedDuration::from_secs(1);
            let states = state.states("p").unwrap();
            let pick = select(strategy, ids, &states, now).unwrap();
            state.record_use("p", &ids[pick], now).unwrap();
            pick
        })
        .collect()
}

#[test]
fn round_robin_rotates() {
    let ids = ids(&["A", "B"]);
    let used = serve(PoolStrategy::RoundRobin, &ids, &MemoryState::default(), 4);
    assert_eq!(used, [0, 1, 0, 1]);
}

#[test]
fn fill_first_stays_on_the_first_and_least_used_balances() {
    let ids = ids(&["A", "B", "C"]);
    assert_eq!(
        serve(PoolStrategy::FillFirst, &ids, &MemoryState::default(), 3),
        [0, 0, 0]
    );
    let state = MemoryState::default();
    state
        .record_use("p", &ids[0], Timestamp::UNIX_EPOCH)
        .unwrap();
    assert_eq!(serve(PoolStrategy::LeastUsed, &ids, &state, 2), [1, 2]);
    let random = serve(PoolStrategy::Random, &ids, &MemoryState::default(), 20);
    assert!(random.iter().all(|&i| i < 3));
}

#[test]
fn a_benched_key_is_skipped_until_its_cooldown_ends() {
    let ids = ids(&["A", "B"]);
    let state = MemoryState::default();
    let now = Timestamp::from_second(1_800_000_000).unwrap();
    let throttled = ProviderError {
        status: Some(429),
        ..ProviderError::new(ErrorClass::RateLimit, "slow down")
    };
    let (until, reason) = cooldown(&throttled, false, now).unwrap();
    assert_eq!(reason, "rate_limit");
    assert_eq!(until, later(now, EXHAUSTED_BENCH));
    state
        .bench("p", &ids[0], Some(until), Some(reason))
        .unwrap();
    let states = state.states("p").unwrap();
    let last_second = later(now, Duration::from_secs(3599));
    for at in [now, later(now, Duration::from_mins(59)), last_second] {
        assert_eq!(select(PoolStrategy::FillFirst, &ids, &states, at), Some(1));
    }
    assert_eq!(
        select(PoolStrategy::FillFirst, &ids, &states, until),
        Some(0)
    );
    state
        .bench("p", &ids[1], Some(until), Some(reason))
        .unwrap();
    let states = state.states("p").unwrap();
    assert_eq!(select(PoolStrategy::RoundRobin, &ids, &states, now), None);
}

#[test]
fn bench_lengths_follow_the_failure() {
    let now = Timestamp::from_second(1_800_000_000).unwrap();
    let failure = |status: u16, class: ErrorClass| ProviderError {
        status: Some(status),
        ..ProviderError::new(class, "x")
    };
    let len = |e: &ProviderError, sole: bool| {
        cooldown(e, sole, now).map(|(until, _)| until.duration_since(now).as_secs())
    };
    assert_eq!(len(&failure(401, ErrorClass::Auth), false), Some(300));
    assert_eq!(len(&failure(429, ErrorClass::RateLimit), false), Some(3600));
    assert_eq!(len(&failure(402, ErrorClass::Billing), false), Some(3600));
    assert_eq!(len(&failure(400, ErrorClass::Billing), false), Some(3600));
    assert_eq!(len(&failure(500, ErrorClass::ServerError), false), None);
    // Sole credential throttled recovers quickly; billing keeps the bench.
    assert_eq!(len(&failure(429, ErrorClass::RateLimit), true), Some(60));
    assert_eq!(len(&failure(401, ErrorClass::Auth), true), Some(60));
    assert_eq!(len(&failure(402, ErrorClass::Billing), true), Some(3600));
    // A provider reset time in the future wins.
    let mut reset = failure(429, ErrorClass::RateLimit);
    reset.reset_at = Some(now + SignedDuration::from_mins(10));
    assert_eq!(len(&reset, false), Some(600));
    assert_eq!(len(&reset, true), Some(60));
    reset.reset_at = Some(now - SignedDuration::from_mins(10));
    assert_eq!(
        len(&reset, false),
        Some(3600),
        "a past reset time is ignored"
    );
}

#[test]
fn members_are_the_default_key_then_labelled_keys() {
    let config = Config::default();
    let registry = ProviderRegistry::for_profile(&config, Vec::new()).unwrap();
    let openrouter = registry.lookup("openrouter").unwrap();
    let names: Vec<String> = [
        "OPENROUTER_API_KEY_WORK",
        "OPENAI_API_KEY",
        "OPENROUTER_API_KEY",
        "OPENROUTER_API_KEY_A_B",
    ]
    .map(str::to_owned)
    .to_vec();
    let members = key_members(openrouter, &names);
    let labels: Vec<&str> = members.iter().map(|m| m.label.as_str()).collect();
    assert_eq!(labels, ["default", "a-b", "work"]);
    assert_eq!(members[2].secret, "OPENROUTER_API_KEY_WORK");
    assert!(members.iter().all(|m| m.pooled));
}

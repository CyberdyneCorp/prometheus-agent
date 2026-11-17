//! The managed scheduler contract against a real `state.db` (spec:
//! scheduled-jobs, Managed scheduler contract): what is armed, and how a
//! signalled occurrence is claimed at most once.

mod support;

use jiff::tz::TimeZone;
use pmt_config::schema::CronConfig;
use pmt_cron::managed::{self, Fired};
use pmt_cron::ops;
use support::{TestProfile, at, create, every};

/// Scenario: Duplicate external fire (the claim side; the daemon test
/// drives the HTTP endpoint).
#[test]
fn a_fire_delivered_twice_claims_its_occurrence_once() {
    let profile = TestProfile::new();
    let store = profile.store();
    let now = at("2026-11-17T10:00:00Z");
    let job = create(
        &store,
        &profile.scripts(),
        every(3600, "digest"),
        "digest",
        now,
    );
    let arms = managed::arms(&store).unwrap();
    assert_eq!(arms.len(), 1);
    assert_eq!(
        arms[0].dedup_key,
        format!("{}:2026-11-17T11:00:00Z", job.id())
    );
    let fire = (job.id(), arms[0].fire_at_ms);
    let config = CronConfig::default();
    let at_fire = at("2026-11-17T11:00:01Z");
    let first = managed::claim_fire(&store, fire, at_fire, &TimeZone::UTC, &config).unwrap();
    let Fired::Run(claimed) = first else {
        panic!("the first fire claims a run: {first:?}");
    };
    assert_eq!(claimed.1.scheduled_at, arms[0].fire_at_ms);
    assert_eq!(
        managed::claim_fire(&store, fire, at_fire, &TimeZone::UTC, &config).unwrap(),
        Fired::Nothing
    );
    // The job is armed again at its next occurrence.
    let next = managed::arms(&store).unwrap();
    assert_eq!(
        next[0].dedup_key,
        format!("{}:2026-11-17T12:00:00Z", job.id())
    );
}

#[test]
fn stale_unknown_and_paused_fires_start_nothing() {
    let profile = TestProfile::new();
    let store = profile.store();
    let now = at("2026-11-17T10:00:00Z");
    let job = create(
        &store,
        &profile.scripts(),
        every(3600, "digest"),
        "digest",
        now,
    );
    let fire_at = managed::arms(&store).unwrap()[0].fire_at_ms;
    let config = CronConfig::default();
    let claim = |id: &str, at_ms: i64, when: &str| {
        managed::claim_fire(&store, (id, at_ms), at(when), &TimeZone::UTC, &config).unwrap()
    };
    assert_eq!(
        claim(job.id(), fire_at - 3_600_000, "2026-11-17T11:00:00Z"),
        Fired::Nothing,
        "stale"
    );
    assert_eq!(
        claim("000000000000", fire_at, "2026-11-17T11:00:00Z"),
        Fired::Unknown
    );
    ops::pause(&store, job.id(), None, now).unwrap();
    assert!(
        managed::arms(&store).unwrap().is_empty(),
        "a paused job is not armed"
    );
    assert_eq!(
        claim(job.id(), fire_at, "2026-11-17T11:00:00Z"),
        Fired::Nothing
    );
}

/// The managed scheduler decides when an occurrence is signalled: an
/// early fire is claimed as at its instant.
#[test]
fn an_early_fire_is_claimed_at_its_instant() {
    let profile = TestProfile::new();
    let store = profile.store();
    let now = at("2026-11-17T10:00:00Z");
    let job = create(
        &store,
        &profile.scripts(),
        every(3600, "digest"),
        "digest",
        now,
    );
    let fire_at = managed::arms(&store).unwrap()[0].fire_at_ms;
    let fired = managed::claim_fire(
        &store,
        (job.id(), fire_at),
        at("2026-11-17T10:20:00Z"),
        &TimeZone::UTC,
        &CronConfig::default(),
    )
    .unwrap();
    let Fired::Run(claimed) = fired else {
        panic!("an early fire claims its run: {fired:?}");
    };
    assert_eq!(claimed.1.scheduled_at, fire_at);
}

//! The scheduler against a real `state.db` and a fake clock (spec:
//! scheduled-jobs, Job record and storage; Job states and status literals;
//! Grace and missed occurrences; At-most-once occurrence firing; Built-in
//! trigger inside the gateway daemon; Repeat limits and retention).

mod support;

use std::sync::Arc;

use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp};
use pmt_config::schema::CronConfig;
use pmt_cron::clock::{Clock as _, FakeClock};
use pmt_cron::job::{JobSpec, JobState};
use pmt_cron::ops;
use pmt_cron::schedule::Schedule;
use pmt_cron::scheduler::{self, Claimed};
use pmt_store::cron::RunRow;
use pmt_store::leases::Holder;

use support::{TestProfile, at, create, every};

fn tick(profile: &TestProfile, store: &pmt_store::Store, now: Timestamp) -> Vec<RunRow> {
    scheduler::tick(
        store,
        &profile.home,
        now,
        &TimeZone::UTC,
        &CronConfig::default(),
    )
    .unwrap()
    .into_iter()
    .map(|(_, run)| run)
    .collect()
}

/// Scenario: Concurrent edits do not clobber each other.
#[test]
fn a_pause_and_an_advance_by_two_writers_both_land() {
    let profile = TestProfile::new();
    let cli = profile.store();
    let daemon = profile.store();
    let now = at("2026-11-04T10:00:00Z");
    let a = create(&cli, &profile.scripts(), every(3600, "a"), "a", now);
    let b = create(&cli, &profile.scripts(), every(3600, "b"), "b", now);
    let later = at("2026-11-04T11:00:30Z");
    std::thread::scope(|s| {
        s.spawn(|| ops::pause(&cli, a.id(), Some("holiday"), later).unwrap());
        s.spawn(|| tick(&profile, &daemon, later));
    });
    let a = ops::find(&cli, a.id()).unwrap();
    let b = ops::find(&cli, b.id()).unwrap();
    assert_eq!(a.state(), JobState::Paused);
    assert_eq!(a.row.paused_reason.as_deref(), Some("holiday"));
    assert_eq!(b.state(), JobState::Running);
    assert_eq!(b.next_run(), Some(at("2026-11-04T12:00:00Z")));
}

/// Scenario: Uncomputable next run is an error, not completion.
#[test]
fn a_job_whose_zone_disappeared_enters_error_and_stays_listed() {
    let profile = TestProfile::new();
    let store = profile.store();
    let now = at("2026-11-04T10:00:00Z");
    let mut spec = JobSpec::new(
        Schedule::Cron {
            expr: "0 9 * * *".into(),
        },
        "x",
    );
    spec.timezone = Some("Europe/Lisbon".into());
    let job = create(&store, &profile.scripts(), spec, "zoned", now);
    // The zone key vanishes from the system database.
    store
        .cron_modify_job(job.id(), |row| {
            row.spec = row.spec.replace("Europe/Lisbon", "Atlantis/Lost");
            row.next_run_at = Some(now.as_millisecond());
            Ok(())
        })
        .unwrap();
    assert!(tick(&profile, &store, now).is_empty());
    let job = ops::find(&store, job.id()).unwrap();
    assert_eq!(job.state(), JobState::Error);
    assert!(
        job.row
            .last_error
            .as_deref()
            .unwrap()
            .contains("Atlantis/Lost")
    );
    assert!(job.row.enabled);
    assert!(store.cron_jobs().unwrap().iter().any(|r| r.id == job.id()));
}

/// Scenario: Explicit zero grace means never catch up.
#[test]
fn an_explicit_zero_grace_skips_the_backlog_and_moves_to_the_next_hour() {
    let profile = TestProfile::new();
    let store = profile.store();
    let mut spec = every(3600, "hourly");
    spec.grace_secs = Some(0);
    let job = create(
        &store,
        &profile.scripts(),
        spec,
        "hourly",
        at("2026-11-04T09:00:00Z"),
    );
    // The host slept through 10:00, 11:00 and 12:00.
    let woke = at("2026-11-04T12:59:30Z");
    assert!(tick(&profile, &store, woke).is_empty());
    let runs = store.cron_runs(job.id(), None, 10).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, "skipped_late");
    assert_eq!(runs[0].skipped, 3);
    let job = ops::find(&store, job.id()).unwrap();
    assert_eq!(job.row.last_status.as_deref(), Some("skipped_late"));
    assert_eq!(job.next_run(), Some(at("2026-11-04T13:00:00Z")));
}

/// Scenario: Default policy collapses the backlog.
#[test]
fn the_default_policy_runs_one_catch_up_for_five_missed_hours() {
    let profile = TestProfile::new();
    let store = profile.store();
    let job = create(
        &store,
        &profile.scripts(),
        every(3600, "hourly"),
        "hourly",
        at("2026-11-04T09:00:00Z"),
    );
    let runs = tick(&profile, &store, at("2026-11-04T14:45:00Z"));
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].lateness.as_deref(), Some("missed"));
    assert_eq!(runs[0].collapsed, 5);
    assert_eq!(store.cron_runs(job.id(), None, 10).unwrap().len(), 1);
    assert_eq!(
        ops::find(&store, job.id()).unwrap().next_run(),
        Some(at("2026-11-04T15:00:00Z"))
    );
}

/// Scenario: Two triggers for one occurrence.
#[test]
fn two_triggers_racing_on_one_occurrence_fire_it_once() {
    let profile = TestProfile::new();
    let ticker = profile.store();
    let manual = profile.store();
    let job = create(
        &ticker,
        &profile.scripts(),
        every(3600, "x"),
        "x",
        at("2026-11-04T09:00:00Z"),
    );
    let due = at("2026-11-04T10:00:05Z");
    let fired: Vec<Vec<RunRow>> = std::thread::scope(|s| {
        let a = s.spawn(|| tick(&profile, &ticker, due));
        let b = s.spawn(|| tick(&profile, &manual, due));
        vec![a.join().unwrap(), b.join().unwrap()]
    });
    assert_eq!(fired.iter().map(Vec::len).sum::<usize>(), 1);
    assert_eq!(ticker.cron_runs(job.id(), None, 10).unwrap().len(), 1);
}

/// Scenario: Crash mid-run is not retried; next-run computation across a
/// restart with a fake clock.
#[test]
fn a_run_interrupted_by_a_crash_is_failed_on_restart_and_not_fired_again() {
    let profile = TestProfile::new();
    let clock = FakeClock::new(at("2026-11-04T08:30:00Z"));
    let job = {
        let store = profile.store();
        let job = create(
            &store,
            &profile.scripts(),
            every(3600, "x"),
            "x",
            clock.now(),
        );
        clock.set(at("2026-11-04T09:30:00Z"));
        let runs = tick(&profile, &store, clock.now());
        assert_eq!(runs.len(), 1);
        // The process executing it dies: its owner is a dead pid.
        store
            .cron_update_run(
                &runs[0].id,
                |r| {
                    r.owner = Some(Holder {
                        pid: u32::MAX - 7,
                        process_start: 1,
                        host_id: pmt_core::runtime::host_id(),
                    });
                    Ok(())
                },
                |_, _| Ok(()),
            )
            .unwrap();
        job
    };
    // The daemon restarts: a new store handle, recovery, the same instant.
    let store = profile.store();
    let finalized = scheduler::recover(&store).unwrap();
    assert_eq!(finalized.len(), 1);
    let run = store.cron_run(&finalized[0]).unwrap().unwrap();
    assert_eq!(
        (run.status.as_str(), run.error.as_deref()),
        ("failed", Some("interrupted"))
    );
    assert!(tick(&profile, &store, clock.now()).is_empty());
    assert_eq!(store.cron_runs(job.id(), None, 10).unwrap().len(), 1);
    let job = ops::find(&store, job.id()).unwrap();
    assert_eq!(job.state(), JobState::Scheduled);
    // The schedule goes on from the next hour, not from the restart.
    clock.advance(SignedDuration::from_mins(60));
    let runs = tick(&profile, &store, clock.now());
    assert_eq!(runs.len(), 1);
    assert_eq!(
        runs[0].scheduled_at,
        at("2026-11-04T10:30:00Z").as_millisecond()
    );
}

/// Scenario: Emergency stop halts dispatch.
#[test]
fn nothing_is_dispatched_while_the_emergency_stop_is_on() {
    let profile = TestProfile::new();
    let store = profile.store();
    let job = create(
        &store,
        &profile.scripts(),
        every(3600, "x"),
        "x",
        at("2026-11-04T09:00:00Z"),
    );
    pmt_core::pause::pause(&profile.home, Some("incident"), "test").unwrap();
    let due = at("2026-11-04T10:00:10Z");
    assert!(tick(&profile, &store, due).is_empty());
    assert!(store.cron_runs(job.id(), None, 10).unwrap().is_empty());
    assert_eq!(
        ops::find(&store, job.id()).unwrap().next_run(),
        Some(at("2026-11-04T10:00:00Z"))
    );
    pmt_core::pause::resume(&profile.home).unwrap();
    assert_eq!(tick(&profile, &store, due).len(), 1);
}

#[test]
fn an_occurrence_due_while_the_previous_run_is_active_is_recorded_skipped() {
    let profile = TestProfile::new();
    let store = profile.store();
    let job = create(
        &store,
        &profile.scripts(),
        every(3600, "x"),
        "x",
        at("2026-11-04T09:00:00Z"),
    );
    assert_eq!(tick(&profile, &store, at("2026-11-04T10:00:01Z")).len(), 1);
    assert!(tick(&profile, &store, at("2026-11-04T11:00:01Z")).is_empty());
    let runs = store.cron_runs(job.id(), None, 10).unwrap();
    assert_eq!(runs[0].status, "skipped_late");
    assert_eq!(runs[0].reason.as_deref(), Some(scheduler::STILL_ACTIVE));
    assert_eq!(
        ops::find(&store, job.id()).unwrap().state(),
        JobState::Running
    );
}

#[test]
fn a_missed_one_shot_is_recorded_skipped_and_completed() {
    let profile = TestProfile::new();
    let store = profile.store();
    let spec = JobSpec::new(
        Schedule::Once {
            at_ms: at("2026-11-04T10:00:00Z").as_millisecond(),
        },
        "once",
    );
    let job = create(
        &store,
        &profile.scripts(),
        spec,
        "once",
        at("2026-11-04T09:00:00Z"),
    );
    assert!(tick(&profile, &store, at("2026-11-04T11:00:00Z")).is_empty());
    let job = ops::find(&store, job.id()).unwrap();
    assert_eq!(job.state(), JobState::Completed);
    assert!(!job.row.enabled);
    assert_eq!(job.row.last_status.as_deref(), Some("skipped_late"));
    let claimed = scheduler::claim(
        &store,
        &job,
        at("2026-11-04T12:00:00Z"),
        &TimeZone::UTC,
        &CronConfig::default(),
    )
    .unwrap();
    assert_eq!(claimed, Claimed::Nothing);
    let _ = Arc::new(());
}

#[test]
fn retention_drops_old_runs_and_completed_jobs_but_keeps_recent_ones() {
    let profile = TestProfile::new();
    let store = profile.store();
    let spec = JobSpec::new(
        Schedule::Once {
            at_ms: at("2026-11-04T10:00:00Z").as_millisecond(),
        },
        "once",
    );
    let job = create(
        &store,
        &profile.scripts(),
        spec,
        "once",
        at("2026-11-04T09:00:00Z"),
    );
    // Missed: recorded skipped and completed at the (real) claim time.
    assert!(tick(&profile, &store, at("2026-11-04T11:00:00Z")).is_empty());
    let config = CronConfig {
        run_retention_secs: 3600,
        completed_retention_secs: 3600,
        ..CronConfig::default()
    };
    // Run rows carry their real creation time, the job its (fake) completion.
    let soon = at("2026-11-04T11:30:00Z");
    scheduler::prune(&store, soon, &config).unwrap();
    assert_eq!(store.cron_runs(job.id(), None, 10).unwrap().len(), 1);
    assert!(store.cron_job(job.id()).unwrap().is_some());
    let later = Timestamp::now()
        .checked_add(SignedDuration::from_hours(48))
        .unwrap();
    scheduler::prune(&store, later, &config).unwrap();
    assert!(store.cron_job(job.id()).unwrap().is_none());
    assert!(store.cron_runs(job.id(), None, 10).unwrap().is_empty());
    // 0 keeps forever.
    let keep = CronConfig {
        run_retention_secs: 0,
        completed_retention_secs: 0,
        ..CronConfig::default()
    };
    scheduler::prune(&store, later, &keep).unwrap();
}

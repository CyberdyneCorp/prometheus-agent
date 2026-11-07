//! What follows a failed run, on a fake clock with a scripted runner and
//! a recording sender (spec: scheduled-jobs, Failure streak and review
//! nudge; Automatic re-runs when the model was unreachable; Hold through a
//! closed provider usage window; Failure incidents with alert-once and
//! acknowledge; Failure notice routing; Agent-declared run failure).

mod support;

use jiff::{SignedDuration, Timestamp};
use pmt_config::schema::CronConfig;
use pmt_cron::exec::AgentEnd;
use pmt_cron::job::Job;
use pmt_store::Store;

use support::{Outbox, Scripted, TestProfile, at, create, every, tick_all};

fn job(
    profile: &TestProfile,
    store: &Store,
    every_secs: u64,
    edit: impl FnOnce(&mut pmt_cron::job::JobSpec),
) -> Job {
    let mut spec = every(every_secs, "morning brief");
    spec.deliver = vec!["telegram".into()];
    edit(&mut spec);
    create(
        store,
        &profile.scripts(),
        spec,
        "brief",
        at("2026-11-03T08:00:00Z"),
    )
}

fn plus(t: Timestamp, secs: i64) -> Timestamp {
    t + SignedDuration::from_secs(secs)
}

fn failing(errors: &[&str]) -> Scripted {
    let runner = Scripted::default();
    *runner.queue.lock().unwrap() = errors
        .iter()
        .map(|e| AgentEnd::Failed((*e).to_owned()))
        .collect();
    runner
}

/// Scenario: Third consecutive failure nudges.
#[test]
fn the_third_consecutive_failure_nudges() {
    let profile = TestProfile::new();
    let store = profile.store();
    let job = job(&profile, &store, 3600, |_| {});
    let runner = failing(&["disk full", "token expired", "quota exceeded"]);
    let outbox = Outbox::default();
    let config = CronConfig::default();
    let start = at("2026-11-03T09:00:00Z");
    for hour in 0..3 {
        let runs = tick_all(
            &profile,
            &store,
            &runner,
            &outbox,
            &config,
            plus(start, hour * 3600),
        );
        assert_eq!(runs[0].status, "failed");
    }
    let bodies = outbox.bodies();
    assert_eq!(bodies.len(), 3);
    assert!(!bodies[1].contains("in a row"));
    assert!(
        bodies[2].contains("This job has failed 3 runs in a row."),
        "{}",
        bodies[2]
    );
    assert!(bodies[2].contains(&format!("prometheus cron pause {}", job.id())));
    assert_eq!(store.cron_job(job.id()).unwrap().unwrap().failure_streak, 3);
    // A success resets the streak.
    *runner.answer.lock().unwrap() = Some(AgentEnd::Completed("fine".into()));
    tick_all(
        &profile,
        &store,
        &runner,
        &outbox,
        &config,
        plus(start, 3 * 3600),
    );
    assert_eq!(store.cron_job(job.id()).unwrap().unwrap().failure_streak, 0);
}

/// Scenario: Laptop wakes before Wi-Fi.
#[test]
fn an_unreachable_model_is_re_run_without_a_failure_notice() {
    let profile = TestProfile::new();
    let store = profile.store();
    let job = job(&profile, &store, 86_400, |_| {});
    let runner = Scripted::answering("today's brief");
    *runner.queue.lock().unwrap() = vec![AgentEnd::Unreachable(
        "dns error: failed to lookup address".into(),
    )];
    let outbox = Outbox::default();
    let config = CronConfig::default();
    let first = tick_all(
        &profile,
        &store,
        &runner,
        &outbox,
        &config,
        at("2026-11-04T08:00:00Z"),
    );
    assert_eq!(first[0].status, "failed");
    // The network returns at 08:03: nothing is due yet.
    assert!(
        tick_all(
            &profile,
            &store,
            &runner,
            &outbox,
            &config,
            at("2026-11-04T08:03:00Z")
        )
        .is_empty()
    );
    let rerun = tick_all(
        &profile,
        &store,
        &runner,
        &outbox,
        &config,
        at("2026-11-04T08:05:00Z"),
    );
    assert_eq!(rerun.len(), 1);
    assert_eq!(rerun[0].trigger, "retry");
    assert_eq!(rerun[0].retry_of.as_deref(), Some(first[0].id.as_str()));
    assert_eq!(
        rerun[0].scheduled_at,
        at("2026-11-04T08:05:00Z").as_millisecond()
    );
    let bodies = outbox.bodies();
    assert_eq!(bodies.len(), 1, "{bodies:?}");
    assert!(bodies[0].contains("today's brief"));
    let row = store.cron_job(job.id()).unwrap().unwrap();
    assert_eq!(row.completed, 1, "a re-run does not count as an occurrence");
    assert_eq!((row.retry_at, row.retry_step), (None, 0));
    assert_eq!(row.state, "scheduled");
}

/// After the ladder (5, 15, 30 minutes) a normal notice is delivered.
#[test]
fn an_exhausted_ladder_delivers_the_failure_notice() {
    let profile = TestProfile::new();
    let store = profile.store();
    job(&profile, &store, 86_400, |_| {});
    let runner = Scripted::default();
    *runner.answer.lock().unwrap() = Some(AgentEnd::Unreachable("network unreachable".into()));
    let outbox = Outbox::default();
    let config = CronConfig::default();
    let mut now = at("2026-11-04T08:00:00Z");
    tick_all(&profile, &store, &runner, &outbox, &config, now);
    for delay in [300, 900, 1800] {
        now = plus(now, delay);
        assert!(outbox.bodies().is_empty());
        assert_eq!(
            tick_all(&profile, &store, &runner, &outbox, &config, now)[0].trigger,
            "retry"
        );
    }
    let bodies = outbox.bodies();
    assert_eq!(bodies.len(), 1);
    assert!(
        bodies[0].contains("failed: network unreachable"),
        "{}",
        bodies[0]
    );
}

/// Scenario: Sub-hourly job does not alert every tick.
#[test]
fn a_closed_usage_window_holds_the_job_with_one_notice() {
    let profile = TestProfile::new();
    let store = profile.store();
    let job = job(&profile, &store, 900, |_| {});
    let start = at("2026-11-03T08:15:00Z");
    let reopens = plus(start, 5 * 3600);
    let runner = Scripted::answering("ok again");
    *runner.queue.lock().unwrap() = vec![AgentEnd::UsageClosed(reopens.as_millisecond())];
    let outbox = Outbox::default();
    let config = CronConfig::default();
    let first = tick_all(&profile, &store, &runner, &outbox, &config, start);
    assert_eq!(first[0].status, "failed");
    assert!(
        first[0]
            .error
            .as_deref()
            .unwrap()
            .starts_with("usage window closed until")
    );
    let mut now = start;
    while now < plus(reopens, -1) {
        now = plus(now, 900).min(plus(reopens, -1));
        assert!(
            tick_all(&profile, &store, &runner, &outbox, &config, now).is_empty(),
            "ran at {now}"
        );
    }
    assert_eq!(outbox.bodies().len(), 1);
    assert!(outbox.bodies()[0].contains("is held"));
    assert_eq!(
        store.cron_job(job.id()).unwrap().unwrap().quota_hold_until,
        Some(reopens.as_millisecond())
    );
    let after = tick_all(&profile, &store, &runner, &outbox, &config, reopens);
    assert_eq!(store.cron_run(&after[0].id).unwrap().unwrap().status, "ok");
    assert_eq!(
        store.cron_job(job.id()).unwrap().unwrap().quota_hold_until,
        None
    );
    assert_eq!(store.cron_runs(job.id(), None, 100).unwrap().len(), 2);
}

/// Scenario: Same error alerts once.
#[test]
fn the_same_error_alerts_once() {
    let profile = TestProfile::new();
    let store = profile.store();
    let job = job(&profile, &store, 3600, |_| {});
    let runner = Scripted::default();
    *runner.answer.lock().unwrap() = Some(AgentEnd::Failed("connection refused".into()));
    let outbox = Outbox::default();
    let config = CronConfig::default();
    let start = at("2026-11-03T09:00:00Z");
    for hour in 0..4 {
        tick_all(
            &profile,
            &store,
            &runner,
            &outbox,
            &config,
            plus(start, hour * 3600),
        );
    }
    assert_eq!(outbox.bodies().len(), 1);
    assert_eq!(
        store.cron_runs(job.id(), Some("failed"), 10).unwrap().len(),
        4
    );
    let incidents = store.cron_incidents(None, Some(job.id())).unwrap();
    assert_eq!(incidents.len(), 1);
    assert_eq!(
        (incidents[0].state.as_str(), incidents[0].occurrences),
        ("alerted", 4)
    );
    // A different error opens its own incident and alerts at once.
    *runner.answer.lock().unwrap() = Some(AgentEnd::Failed("disk full".into()));
    tick_all(
        &profile,
        &store,
        &runner,
        &outbox,
        &config,
        plus(start, 4 * 3600),
    );
    assert_eq!(outbox.bodies().len(), 2);
    // Past `failure_repeat_alert_secs` (6 h) the first one reminds once.
    *runner.answer.lock().unwrap() = Some(AgentEnd::Failed("connection refused".into()));
    for hour in 5..9 {
        tick_all(
            &profile,
            &store,
            &runner,
            &outbox,
            &config,
            plus(start, hour * 3600),
        );
    }
    assert_eq!(outbox.bodies().len(), 3);
}

/// Scenario: Acknowledged signature stays silent after success.
#[test]
fn an_acknowledged_signature_stays_silent_after_success() {
    let profile = TestProfile::new();
    let store = profile.store();
    let job = job(&profile, &store, 3600, |_| {});
    let runner = failing(&["connection refused"]);
    let outbox = Outbox::default();
    let config = CronConfig::default();
    let start = at("2026-11-03T09:00:00Z");
    tick_all(&profile, &store, &runner, &outbox, &config, start);
    assert_eq!(outbox.bodies().len(), 1);
    let incident = store
        .cron_incidents(None, Some(job.id()))
        .unwrap()
        .remove(0);
    store
        .cron_ack_incident(&incident.id, start.as_millisecond())
        .unwrap();
    tick_all(
        &profile,
        &store,
        &runner,
        &outbox,
        &config,
        plus(start, 3600),
    );
    let delivered = outbox.bodies().len();
    *runner.queue.lock().unwrap() = vec![AgentEnd::Failed("connection refused".into())];
    tick_all(
        &profile,
        &store,
        &runner,
        &outbox,
        &config,
        plus(start, 2 * 3600),
    );
    assert_eq!(
        outbox.bodies().len(),
        delivered,
        "no notice after the acknowledgement"
    );
    assert_eq!(store.cron_job(job.id()).unwrap().unwrap().failure_streak, 1);
    assert_eq!(store.cron_incidents(Some("closed"), None).unwrap().len(), 1);
}

/// Scenario: Failures go to an ops channel.
#[test]
fn failures_go_to_the_failure_targets() {
    let profile = TestProfile::new();
    let store = profile.store();
    job(&profile, &store, 3600, |spec| {
        spec.deliver = vec!["slack:C_TEAM".into()];
        spec.failure_deliver = vec!["telegram".into()];
    });
    let runner = failing(&["upstream 502"]);
    let outbox = Outbox::default();
    let config = CronConfig::default();
    tick_all(
        &profile,
        &store,
        &runner,
        &outbox,
        &config,
        at("2026-11-03T09:00:00Z"),
    );
    let sent = outbox.sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0, "telegram");
    // Successful output still follows `deliver`.
    tick_all(
        &profile,
        &store,
        &runner,
        &outbox,
        &config,
        at("2026-11-03T10:00:00Z"),
    );
    assert_eq!(outbox.sent.lock().unwrap()[1].0, "slack:C_TEAM");
}

/// Scenario: Agent reports a failed subtask.
#[test]
fn an_agent_declared_failure_fails_the_run() {
    let profile = TestProfile::new();
    let store = profile.store();
    let job = job(&profile, &store, 3600, |_| {});
    let answer = "[CRON_FAILURE]\nexport subagent exited: disk full";
    let runner = Scripted::answering(answer);
    let outbox = Outbox::default();
    let config = CronConfig::default();
    let run = tick_all(
        &profile,
        &store,
        &runner,
        &outbox,
        &config,
        at("2026-11-03T09:00:00Z"),
    )
    .remove(0);
    assert_eq!(run.status, "failed");
    assert_eq!(run.output.as_deref(), Some(answer));
    let row = store.cron_job(job.id()).unwrap().unwrap();
    assert_eq!(row.last_status.as_deref(), Some("failed"));
    assert_eq!(
        row.last_error.as_deref(),
        Some("export subagent exited: disk full")
    );
    assert_eq!(row.failure_streak, 1);
    assert!(outbox.bodies()[0].contains("failed: export subagent exited: disk full"));
    // The marker anywhere else leaves the run successful.
    *runner.answer.lock().unwrap() = Some(AgentEnd::Completed(
        "note: [CRON_FAILURE] is a marker".into(),
    ));
    let ok = tick_all(
        &profile,
        &store,
        &runner,
        &outbox,
        &config,
        at("2026-11-03T10:00:00Z"),
    )
    .remove(0);
    assert_eq!(store.cron_run(&ok.id).unwrap().unwrap().status, "ok");
}

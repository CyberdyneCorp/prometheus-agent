//! The trigger-independent half of scheduling (spec: scheduled-jobs,
//! At-most-once occurrence firing; Grace and missed occurrences; Job
//! states and status literals).
//!
//! A trigger (the daemon's ticker, `prometheus cron tick`) only decides
//! *when* to look; [`tick`] decides what is due and claims it: the run row
//! is inserted under its unique (job, instant) key and the job advanced in
//! one transaction before anything executes, so racing triggers fire an
//! occurrence once. [`recover`] finalizes the runs of a process that died.

use std::path::Path;

use jiff::Timestamp;
use jiff::tz::TimeZone;
use pmt_config::schema::CronConfig;
use pmt_core::error::Error;
use pmt_store::Store;
use pmt_store::cron::{Claim, JobRow, RunRow};
use pmt_store::leases::Holder;

use crate::grace::{self, Plan, Policy};
use crate::job::{Job, JobState, Status};
use crate::schedule::at_ms;

/// Most occurrences counted for one backlog.
const MAX_BACKLOG: usize = 100_000;

/// The reason of a run skipped while its job still runs.
pub const STILL_ACTIVE: &str = "previous run still active";

/// A new run id.
#[must_use]
pub fn new_run_id() -> String {
    format!("run_{}", ulid::Ulid::generate())
}

/// A run row for `job` at `scheduled`.
#[must_use]
pub fn run_row(job: &Job, scheduled: Timestamp, trigger: &str, status: Status) -> RunRow {
    RunRow {
        id: new_run_id(),
        job_id: job.id().to_owned(),
        job_name: job.row.name.clone(),
        scheduled_at: scheduled.as_millisecond(),
        trigger: trigger.to_owned(),
        status: status.as_str().to_owned(),
        created_at: Timestamp::now().as_millisecond(),
        ..RunRow::default()
    }
}

/// The occurrences of `job` from its `next_run_at` up to `now`.
fn due_occurrences(job: &Job, now: Timestamp, zone: &TimeZone) -> Result<Vec<Timestamp>, String> {
    let Some(mut at) = job.next_run() else {
        return Ok(Vec::new());
    };
    let mut due = Vec::new();
    while at <= now && due.len() < MAX_BACKLOG {
        due.push(at);
        match job.spec.schedule.next_after(at, at, zone)? {
            Some(next) => at = next,
            None => break,
        }
    }
    Ok(due)
}

/// Puts `row` in `error` with `message` (it stays listed and enabled
/// rows keep their flag; it is never marked completed).
fn set_error(row: &mut JobRow, message: &str) {
    JobState::Error.as_str().clone_into(&mut row.state);
    row.last_error = Some(message.to_owned());
    row.next_run_at = None;
}

/// Records `job` as unschedulable.
fn fail_job(store: &Store, job: &Job, message: &str) -> Result<(), Error> {
    tracing::warn!(
        job = job.id(),
        error = message,
        "cron job cannot be scheduled"
    );
    store
        .cron_modify_job(job.id(), |row| {
            set_error(row, message);
            Ok(())
        })
        .map(|_| ())
}

/// The job's next instant after a plan: an ordinary fire moves to the
/// occurrence after it (possibly due already), a catch-up or skip to the
/// first one after `now`.
fn next_after_plan(
    job: &Job,
    plan: &Plan,
    now: Timestamp,
    zone: &TimeZone,
) -> Result<Option<Timestamp>, String> {
    let schedule = &job.spec.schedule;
    match plan {
        Plan::Fire {
            scheduled,
            collapsed: 0,
            ..
        } => schedule.next_after(*scheduled, *scheduled, zone),
        Plan::Fire { scheduled, .. } | Plan::Skip { scheduled, .. } => {
            schedule.next_after(now, *scheduled, zone)
        }
    }
}

/// How the job row changes when a plan is claimed.
fn advance(row: &mut JobRow, job: &Job, plan: &Plan, next: Option<Timestamp>, now: Timestamp) {
    row.next_run_at = next.map(Timestamp::as_millisecond);
    match plan {
        Plan::Fire { .. } => {
            JobState::Running.as_str().clone_into(&mut row.state);
            row.last_run_at = Some(now.as_millisecond());
        }
        Plan::Skip { .. } => {
            row.last_status = Some(Status::SkippedLate.as_str().to_owned());
        }
    }
    if next.is_none() {
        if job.spec.schedule.recurring() {
            set_error(row, "the schedule has no next instant");
        } else if matches!(plan, Plan::Skip { .. }) {
            complete(row, now);
        }
    }
}

/// Marks `row` completed: disabled, no next run.
pub fn complete(row: &mut JobRow, now: Timestamp) {
    JobState::Completed.as_str().clone_into(&mut row.state);
    row.enabled = false;
    row.next_run_at = None;
    row.completed_at = Some(now.as_millisecond());
}

/// The run record of a plan.
fn record(job: &Job, plan: &Plan, lateness_still_active: bool) -> RunRow {
    match plan {
        Plan::Fire {
            scheduled,
            lateness,
            collapsed,
        } if !lateness_still_active => {
            let mut run = run_row(job, *scheduled, "schedule", Status::Ok);
            "running".clone_into(&mut run.status);
            // The claiming process owns it: if it dies before or during
            // execution, the next scheduler start finalizes the run.
            run.owner = Some(Holder::current());
            run.lateness = Some(lateness.as_str().to_owned());
            run.collapsed = *collapsed;
            run
        }
        Plan::Fire {
            scheduled,
            lateness,
            collapsed,
        } => {
            let mut run = run_row(job, *scheduled, "schedule", Status::SkippedLate);
            run.lateness = Some(lateness.as_str().to_owned());
            run.skipped = (*collapsed).max(1);
            run.reason = Some(STILL_ACTIVE.to_owned());
            run.ended_at = Some(run.created_at);
            run
        }
        Plan::Skip { scheduled, skipped } => {
            let mut run = run_row(job, *scheduled, "schedule", Status::SkippedLate);
            run.lateness = Some("missed".to_owned());
            run.skipped = *skipped;
            run.ended_at = Some(run.created_at);
            run
        }
    }
}

/// What one claim attempt did.
#[derive(Debug, Clone, PartialEq)]
pub enum Claimed {
    /// Nothing was due (or another trigger won).
    Nothing,
    /// A skip was recorded; the job may still be due.
    Skipped,
    /// A run was claimed and must now execute.
    Run(Box<(Job, RunRow)>),
}

/// Claims what is due of `job` at `now`.
///
/// # Errors
///
/// Store errors.
pub fn claim(
    store: &Store,
    job: &Job,
    now: Timestamp,
    profile_zone: &TimeZone,
    config: &CronConfig,
) -> Result<Claimed, Error> {
    let zone = match job.spec.zone(profile_zone) {
        Ok(zone) => zone,
        Err(message) => return fail_job(store, job, &message).map(|()| Claimed::Nothing),
    };
    let due = match due_occurrences(job, now, &zone) {
        Ok(due) if due.is_empty() => return Ok(Claimed::Nothing),
        Ok(due) => due,
        Err(message) => return fail_job(store, job, &message).map(|()| Claimed::Nothing),
    };
    let schedule = &job.spec.schedule;
    let policy = Policy {
        grace_secs: grace::grace_secs(job.spec.grace_secs, schedule.period_secs(due[0], &zone)),
        explicit_grace: job.spec.grace_secs.is_some(),
        catch_up_missed: config.catch_up_missed,
        recurring: schedule.recurring(),
    };
    let plan = grace::plan(&due, now, policy);
    let next = match next_after_plan(job, &plan, now, &zone) {
        Ok(next) => next,
        Err(message) => return fail_job(store, job, &message).map(|()| Claimed::Nothing),
    };
    let still_active = matches!(plan, Plan::Fire { .. }) && job.state() == JobState::Running;
    let run = record(job, &plan, still_active);
    let effective = if still_active {
        Plan::Skip {
            scheduled: at_ms(run.scheduled_at),
            skipped: run.skipped,
        }
    } else {
        plan
    };
    let claimed = store.cron_claim(job.id(), job.row.next_run_at, &run, |row| {
        advance(row, job, &effective, next, now);
        if still_active {
            // The running job keeps running.
            JobState::Running.as_str().clone_into(&mut row.state);
        }
        Ok(())
    })?;
    match (claimed, &effective) {
        (Claim::Lost, _) => {
            tracing::debug!(
                job = job.id(),
                at = run.scheduled_at,
                "occurrence already claimed"
            );
            Ok(Claimed::Nothing)
        }
        (Claim::Claimed(_), Plan::Skip { .. }) => Ok(Claimed::Skipped),
        (Claim::Claimed(row), Plan::Fire { .. }) => {
            let job = Job::from_row(*row)?;
            Ok(Claimed::Run(Box::new((job, run))))
        }
    }
}

/// Jobs the scheduler looks at: enabled, not in error, with a next run
/// at or before `now`.
///
/// # Errors
///
/// Store errors.
pub fn due_jobs(store: &Store, now: Timestamp) -> Result<Vec<Job>, Error> {
    let now = now.as_millisecond();
    store
        .cron_jobs()?
        .into_iter()
        .filter(|row| {
            row.enabled
                && row.state != JobState::Error.as_str()
                && row.next_run_at.is_some_and(|at| at <= now)
        })
        .map(Job::from_row)
        .collect()
}

/// One evaluation of every due job of a profile: nothing while the
/// emergency stop under `home` is on; otherwise every due occurrence is
/// claimed, and the claimed runs (plus manual runs queued for this
/// scheduler) are returned for execution.
///
/// # Errors
///
/// Store errors.
pub fn tick(
    store: &Store,
    home: &Path,
    now: Timestamp,
    profile_zone: &TimeZone,
    config: &CronConfig,
) -> Result<Vec<(Job, RunRow)>, Error> {
    if pmt_core::pause::active(home).is_some() {
        tracing::debug!("cron dispatch held by the emergency stop");
        return Ok(Vec::new());
    }
    let mut runs = Vec::new();
    for job in due_jobs(store, now)? {
        let mut job = job;
        // A skip may leave a fresh occurrence due: look once more.
        for _ in 0..2 {
            match claim(store, &job, now, profile_zone, config)? {
                Claimed::Run(claimed) => {
                    runs.push(*claimed);
                    break;
                }
                Claimed::Nothing => break,
                Claimed::Skipped => match store.cron_job(job.id())? {
                    Some(row) => job = Job::from_row(row)?,
                    None => break,
                },
            }
        }
    }
    for run in store.cron_runs_in(&["queued"])? {
        if let Some(row) = store.cron_job(&run.job_id)? {
            runs.push((Job::from_row(row)?, run));
        }
    }
    Ok(runs)
}

/// Finalizes runs whose owner process on this host died: `failed` with
/// `interrupted`, never re-fired; their jobs leave `running`. Delivery
/// rows left `sending` become `unknown`. Returns the finalized run ids.
///
/// # Errors
///
/// Store errors.
pub fn recover(store: &Store) -> Result<Vec<String>, Error> {
    let host = pmt_core::runtime::host_id();
    let mut finalized = Vec::new();
    for run in store.cron_runs_in(&["running"])? {
        let dead = run
            .owner
            .as_ref()
            .is_none_or(|o: &Holder| o.is_dead_local(&host));
        if !dead {
            continue;
        }
        store.cron_update_run(
            &run.id,
            |r| {
                Status::Failed.as_str().clone_into(&mut r.status);
                r.error = Some("interrupted".to_owned());
                r.ended_at = Some(Timestamp::now().as_millisecond());
                Ok(())
            },
            |r, job| {
                job.last_status = Some(Status::Failed.as_str().to_owned());
                job.last_error = Some("interrupted".to_owned());
                if job.state == JobState::Running.as_str() && r.trigger == "schedule" {
                    JobState::Scheduled.as_str().clone_into(&mut job.state);
                }
                Ok(())
            },
        )?;
        finalized.push(run.id);
    }
    crate::deliver::recover(store)?;
    Ok(finalized)
}

/// Deletes run records and completed jobs past their retention.
///
/// # Errors
///
/// Store errors.
pub fn prune(store: &Store, now: Timestamp, config: &CronConfig) -> Result<(), Error> {
    let before = |secs: u64| {
        now.as_millisecond() - i64::try_from(secs.saturating_mul(1000)).unwrap_or(i64::MAX)
    };
    if config.run_retention_secs > 0 {
        store.cron_delete_runs_before(before(config.run_retention_secs))?;
    }
    if config.completed_retention_secs > 0 {
        store.cron_delete_completed_before(before(config.completed_retention_secs))?;
    }
    Ok(())
}

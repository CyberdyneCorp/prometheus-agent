//! Lifecycle operations shared by the `cron` CLI and the `cronjob` tool
//! (spec: scheduled-jobs, Lifecycle operations; Job payload validation;
//! Canonical schedule echo and confirmation; Delivery targets).

use std::collections::HashMap;
use std::path::Path;
use std::sync::{LazyLock, Mutex, PoisonError};

use jiff::Timestamp;
use jiff::tz::TimeZone;
use pmt_core::error::{Error, codes};
use pmt_store::Store;
use pmt_store::cron::{JobRow, RunRow};
use sha2::{Digest as _, Sha256};

use crate::job::{Job, JobSpec, JobState, Status, new_id, spec_json};
use crate::scan;
use crate::schedule::{Echo, Schedule};
use crate::scheduler::run_row;
use crate::target::JobOrigin;
use crate::validate::{self, Catalog};

/// How long a tool confirmation token stays valid.
pub const CONFIRM_TTL_MS: i64 = 10 * 60 * 1000;

/// What the operations need of a profile.
pub struct Profile<'a> {
    /// Its store.
    pub store: &'a Store,
    /// `<profile>/scripts`.
    pub scripts_dir: &'a Path,
    /// The profile zone (`cron.timezone`, else the host's).
    pub zone: TimeZone,
    /// Its registries.
    pub catalog: &'a dyn Catalog,
}

/// Validates and scans `spec`; returns the echo of its schedule.
///
/// # Errors
///
/// `cron.invalid_job`, `cron.prompt_rejected`.
pub fn check(profile: &Profile<'_>, spec: &JobSpec, now: Timestamp) -> Result<Echo, Error> {
    validate::validate(spec, profile.scripts_dir, profile.catalog)?;
    if let Some(rule) = scan::violation(&spec.prompt) {
        return Err(Error::new(
            &codes::CRON_PROMPT_REJECTED,
            format!("prompt rejected: {rule}"),
        ));
    }
    let zone = spec
        .zone(&profile.zone)
        .map_err(|e| Error::new(&codes::CRON_INVALID_JOB, e))?;
    Ok(Echo::of(&spec.schedule, now, now, &zone))
}

/// The deliveries of a job that names none: its origin chat, else local.
#[must_use]
pub fn default_deliver(origin: Option<&JobOrigin>) -> Vec<String> {
    match origin {
        Some(JobOrigin::Chat { .. }) => vec!["origin".to_owned()],
        _ => vec!["local".to_owned()],
    }
}

fn next_from(spec: &JobSpec, now: Timestamp, zone: &TimeZone) -> Result<Option<i64>, Error> {
    let zone = spec
        .zone(zone)
        .map_err(|e| Error::new(&codes::CRON_INVALID_JOB, e))?;
    spec.schedule
        .next_after(now, now, &zone)
        .map(|next| next.map(Timestamp::as_millisecond))
        .map_err(|e| Error::new(&codes::CRON_INVALID_SCHEDULE, e))
}

/// Saves a checked job.
///
/// # Errors
///
/// As [`check`], store errors.
pub fn create(
    profile: &Profile<'_>,
    mut spec: JobSpec,
    name: Option<String>,
    origin: Option<&JobOrigin>,
    now: Timestamp,
) -> Result<Job, Error> {
    if spec.deliver.is_empty() {
        spec.deliver = default_deliver(origin);
    }
    check(profile, &spec, now)?;
    let now_ms = now.as_millisecond();
    let row = JobRow {
        id: new_id(),
        name: name
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| spec.default_name()),
        spec: spec_json(&spec),
        origin: origin.and_then(|o| serde_json::to_string(o).ok()),
        state: JobState::Scheduled.as_str().to_owned(),
        enabled: true,
        next_run_at: next_from(&spec, now, &profile.zone)?,
        last_run_at: None,
        last_status: None,
        last_error: None,
        completed: 0,
        paused_at: None,
        paused_reason: None,
        alerted: false,
        completed_at: None,
        created_at: now_ms,
        updated_at: now_ms,
    };
    profile.store.cron_insert_job(&row)?;
    Job::from_row(row)
}

/// The job whose id is `key`, else the one job named `key`.
///
/// # Errors
///
/// `cron.job_not_found` (66), `cron.ambiguous_name` (2) listing the ids.
pub fn find(store: &Store, key: &str) -> Result<Job, Error> {
    if let Some(row) = store.cron_job(key)? {
        return Job::from_row(row);
    }
    let mut named: Vec<JobRow> = store
        .cron_jobs()?
        .into_iter()
        .filter(|row| row.name == key)
        .collect();
    match named.len() {
        0 => Err(pmt_store::cron::job_not_found(key)),
        1 => Job::from_row(named.remove(0)),
        _ => {
            let ids: Vec<&str> = named.iter().map(|r| r.id.as_str()).collect();
            Err(Error::new(
                &codes::CRON_AMBIGUOUS_NAME,
                format!(
                    "cron job name '{key}' matches {} jobs: {}",
                    ids.len(),
                    ids.join(", ")
                ),
            )
            .with_hint("name the job by its id"))
        }
    }
}

/// Pauses a job (`enabled = false`, `state = paused`), recording when and
/// why.
///
/// # Errors
///
/// As [`find`]; a completed job cannot be paused.
pub fn pause(store: &Store, key: &str, reason: Option<&str>, now: Timestamp) -> Result<Job, Error> {
    let job = find(store, key)?;
    let row = store.cron_modify_job(job.id(), |row| {
        if row.state == JobState::Completed.as_str() {
            return Err(invalid_state(format!("job {} is completed", row.id)));
        }
        row.enabled = false;
        JobState::Paused.as_str().clone_into(&mut row.state);
        row.paused_at = Some(now.as_millisecond());
        row.paused_reason = reason.map(str::to_owned);
        Ok(())
    })?;
    Job::from_row(row)
}

fn invalid_state(message: String) -> Error {
    Error::new(&codes::CRON_INVALID_STATE, message)
}

/// Resumes a job, recomputing its next run from `now`; a one-shot whose
/// instant has passed is refused.
///
/// # Errors
///
/// As [`find`], `cron.invalid_state`.
pub fn resume(store: &Store, key: &str, now: Timestamp, zone: &TimeZone) -> Result<Job, Error> {
    let job = find(store, key)?;
    if let Schedule::Once { at_ms } = job.spec.schedule
        && at_ms <= now.as_millisecond()
    {
        return Err(invalid_state(format!(
            "cannot resume: one-shot time {} is in the past; use 'prometheus cron edit --schedule'",
            crate::schedule::at_ms(at_ms)
        )));
    }
    let next = next_from(&job.spec, now, zone)?;
    let row = store.cron_modify_job(job.id(), |row| {
        row.enabled = true;
        if row.state != JobState::Running.as_str() {
            JobState::Scheduled.as_str().clone_into(&mut row.state);
        }
        row.paused_at = None;
        row.paused_reason = None;
        row.next_run_at = next;
        Ok(())
    })?;
    Job::from_row(row)
}

/// Applies `edit` to a job's definition (its error is returned as is),
/// re-validates it and, when the
/// schedule changed, recomputes the next run from `now`. Returns the job
/// and the echo of a changed schedule.
///
/// # Errors
///
/// As [`find`] and [`check`].
pub fn update(
    profile: &Profile<'_>,
    key: &str,
    name: Option<&str>,
    edit: impl FnOnce(&mut JobSpec) -> Result<(), Error>,
    now: Timestamp,
) -> Result<(Job, Option<Echo>), Error> {
    let job = find(profile.store, key)?;
    let mut spec = job.spec.clone();
    edit(&mut spec)?;
    let echo = check(profile, &spec, now)?;
    let rescheduled = spec.schedule != job.spec.schedule || spec.timezone != job.spec.timezone;
    let next = if rescheduled {
        next_from(&spec, now, &profile.zone)?
    } else {
        job.row.next_run_at
    };
    let text = spec_json(&spec);
    let row = profile.store.cron_modify_job(job.id(), |row| {
        row.spec.clone_from(&text);
        if let Some(name) = name {
            name.clone_into(&mut row.name);
        }
        if rescheduled {
            row.next_run_at = next;
            // A new schedule brings an errored or completed job back.
            if row.state == JobState::Error.as_str() || row.state == JobState::Completed.as_str() {
                JobState::Scheduled.as_str().clone_into(&mut row.state);
                row.enabled = true;
                row.last_error = None;
                row.completed_at = None;
            }
        }
        Ok(())
    })?;
    Ok((Job::from_row(row)?, rescheduled.then_some(echo)))
}

/// Deletes a job (its runs stay under run retention).
///
/// # Errors
///
/// As [`find`].
pub fn remove(store: &Store, key: &str) -> Result<Job, Error> {
    let job = find(store, key)?;
    store.cron_delete_job(job.id())?;
    Ok(job)
}

/// A manual run record of `key` waiting to execute (`queued`); the job's
/// schedule and state are untouched.
///
/// # Errors
///
/// As [`find`], store errors.
pub fn manual_run(store: &Store, key: &str, now: Timestamp) -> Result<(Job, RunRow), Error> {
    let job = find(store, key)?;
    let mut run = run_row(&job, now, "manual", Status::Ok);
    "queued".clone_into(&mut run.status);
    // Two manual runs in one millisecond still get distinct keys.
    for _ in 0..1000 {
        match store.cron_insert_run(&run) {
            Ok(()) => return Ok((job, run)),
            Err(e) if e.code().code == codes::STORE_CONSTRAINT.code => run.scheduled_at += 1,
            Err(e) => return Err(e),
        }
    }
    Err(Error::new(
        &codes::STORE_CONSTRAINT,
        "no free manual run slot",
    ))
}

static TOKENS: LazyLock<Mutex<HashMap<String, (String, i64)>>> = LazyLock::new(Mutex::default);

fn digest(spec: &JobSpec, name: Option<&str>) -> String {
    let mut hash = Sha256::new();
    hash.update(spec_json(spec));
    hash.update(name.unwrap_or_default());
    crate::job::hex(&hash.finalize())
}

/// A confirmation token for saving exactly `spec` (and `name`) within 10
/// minutes of `now`.
#[must_use]
pub fn confirm_token(spec: &JobSpec, name: Option<&str>, now: Timestamp) -> String {
    let token = format!("cfm_{}", ulid::Ulid::generate());
    let mut tokens = TOKENS.lock().unwrap_or_else(PoisonError::into_inner);
    let now_ms = now.as_millisecond();
    tokens.retain(|_, (_, expires)| *expires > now_ms);
    tokens.insert(token.clone(), (digest(spec, name), now_ms + CONFIRM_TTL_MS));
    token
}

/// Whether `token` confirms saving `spec` at `now` (it is used up).
#[must_use]
pub fn redeem(token: &str, spec: &JobSpec, name: Option<&str>, now: Timestamp) -> bool {
    let mut tokens = TOKENS.lock().unwrap_or_else(PoisonError::into_inner);
    match tokens.remove(token) {
        Some((expected, expires)) => {
            expires > now.as_millisecond() && expected == digest(spec, name)
        }
        None => false,
    }
}

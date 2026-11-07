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

use crate::job::{Job, JobSpec, JobState, SELF, Status, new_id, spec_json};
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
    for source in spec.context_from.iter().filter(|s| *s != SELF) {
        find(profile.store, source).map_err(|e| {
            Error::new(
                &codes::CRON_INVALID_JOB,
                format!("context_from: {}", e.message()),
            )
        })?;
    }
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

/// The reason of a job created paused without one.
pub const DEFAULT_PAUSED_REASON: &str = "Created paused; awaiting operator approval.";

/// How a job is created.
#[derive(Debug, Clone, Default)]
pub struct NewJob<'a> {
    /// Its display name (default: from the prompt).
    pub name: Option<String>,
    /// The creating chat.
    pub origin: Option<&'a JobOrigin>,
    /// Created paused, with this reason (empty: the default reason).
    pub paused: Option<String>,
}

/// `context_from` entries naming the job itself become `self`.
fn own_entries(spec: &mut JobSpec, own: &[&str]) {
    for entry in &mut spec.context_from {
        if own.contains(&entry.as_str()) {
            SELF.clone_into(entry);
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    spec.context_from.retain(|e| seen.insert(e.clone()));
}

/// Saves a checked job; a paused job is stored disabled in its first
/// write, arming nothing.
///
/// # Errors
///
/// As [`check`], store errors.
pub fn create(
    profile: &Profile<'_>,
    mut spec: JobSpec,
    new: NewJob<'_>,
    now: Timestamp,
) -> Result<Job, Error> {
    if spec.deliver.is_empty() {
        spec.deliver = default_deliver(new.origin);
    }
    let name = new
        .name
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| spec.default_name());
    own_entries(&mut spec, &[name.as_str()]);
    check(profile, &spec, now)?;
    let now_ms = now.as_millisecond();
    let paused = new.paused.map(|reason| {
        if reason.trim().is_empty() {
            DEFAULT_PAUSED_REASON.to_owned()
        } else {
            reason
        }
    });
    let next_run_at = match paused {
        Some(_) => None,
        None => next_from(&spec, now, &profile.zone)?,
    };
    let row = JobRow {
        id: new_id(),
        name,
        spec: spec_json(&spec),
        origin: new.origin.and_then(|o| serde_json::to_string(o).ok()),
        state: if paused.is_some() {
            JobState::Paused
        } else {
            JobState::Scheduled
        }
        .as_str()
        .to_owned(),
        enabled: paused.is_none(),
        next_run_at,
        paused_at: paused.as_ref().map(|_| now_ms),
        paused_reason: paused,
        created_at: now_ms,
        updated_at: now_ms,
        ..JobRow::default()
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
    own_entries(&mut spec, &[job.id(), name.unwrap_or(&job.row.name)]);
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
    queue_run(store, key, now, &QueueRun::default())
}

/// How [`queue_run`] queues a run.
#[derive(Debug, Clone, Default)]
pub struct QueueRun {
    /// `manual` (default) or `webhook`.
    pub trigger: Option<&'static str>,
    /// Appended under `## Run Context` to this run's prompt only.
    pub context: Option<String>,
    /// Refuse while a run of the job is queued or running.
    pub exclusive: bool,
    /// Refuse a paused job.
    pub enabled_only: bool,
}

/// Whether a run of `job_id` is queued or running.
///
/// # Errors
///
/// Store errors.
pub fn active(store: &Store, job_id: &str) -> Result<bool, Error> {
    Ok(store
        .cron_runs_in(&["queued", "running"])?
        .iter()
        .any(|r| r.job_id == job_id))
}

/// Queues a run of `key` (`queued`) with `how`'s trigger and context; the
/// job's schedule and state are untouched. An exclusive run is refused
/// while another is queued or running (`cron.already_running`). The
/// context passes the prompt safety scan.
///
/// # Errors
///
/// As [`find`], `cron.already_running`, `cron.invalid_state` (a paused
/// job with `enabled_only`), `cron.prompt_rejected`, store errors.
pub fn queue_run(
    store: &Store,
    key: &str,
    now: Timestamp,
    how: &QueueRun,
) -> Result<(Job, RunRow), Error> {
    let job = find(store, key)?;
    if how.enabled_only && !job.row.enabled {
        return Err(invalid_state(format!("job {} is paused", job.id())));
    }
    if let Some(rule) = how.context.as_deref().and_then(scan::violation) {
        return Err(Error::new(
            &codes::CRON_PROMPT_REJECTED,
            format!("prompt rejected: {rule}"),
        ));
    }
    if how.exclusive && (job.state() == JobState::Running || active(store, job.id())?) {
        return Err(Error::new(
            &codes::CRON_ALREADY_RUNNING,
            format!("job {} ({}) is already running", job.id(), job.row.name),
        ));
    }
    let mut run = run_row(&job, now, how.trigger.unwrap_or("manual"), Status::Ok);
    "queued".clone_into(&mut run.status);
    run.run_context.clone_from(&how.context);
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

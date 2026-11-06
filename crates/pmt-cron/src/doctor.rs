//! The cron doctor (spec: scheduled-jobs, Cron doctor; Pluggable trigger
//! provider): per profile, the trigger in effect and any fallback reason,
//! the tick lease holder, due-but-unfired occurrences, jobs in `error` or
//! `blocked_config`, undelivered outbox rows and every `cron.*` setting
//! with its source.

use jiff::Timestamp;
use pmt_config::ProfileScope;
use pmt_config::schema::{CronConfig, CronProvider};
use pmt_core::error::Error;
use pmt_store::Store;
use serde::Serialize;

use crate::job::{JobState, Status};

/// The `cron.*` keys the doctor reports.
pub const SETTINGS: &[&str] = &[
    "cron.provider",
    "cron.timezone",
    "cron.tick_interval_secs",
    "cron.catch_up_missed",
    "cron.max_parallel_jobs",
    "cron.agent_idle_timeout_secs",
    "cron.max_run_secs",
    "cron.script_timeout_secs",
    "cron.default_toolsets",
    "cron.allow_agent_scheduling",
    "cron.preflight",
    "cron.wrap_response",
    "cron.run_retention_secs",
    "cron.completed_retention_secs",
];

/// The trigger in effect for `config` and why it differs from the
/// configured one: the managed scheduler is not part of this build, so
/// `managed` falls back to `builtin`.
#[must_use]
pub fn effective_provider(config: &CronConfig) -> (CronProvider, Option<String>) {
    match config.provider {
        CronProvider::Builtin => (CronProvider::Builtin, None),
        CronProvider::Managed => (
            CronProvider::Builtin,
            Some(
                "cron.provider = \"managed\" but no managed scheduler is available in this \
                 build; using builtin"
                    .to_owned(),
            ),
        ),
    }
}

/// A job needing attention.
#[derive(Debug, Clone, Serialize)]
pub struct Problem {
    /// The job id.
    pub id: String,
    /// Its name.
    pub name: String,
    /// `error` or `blocked_config`.
    pub kind: String,
    /// Why.
    pub detail: Option<String>,
}

/// An outbox row not delivered.
#[derive(Debug, Clone, Serialize)]
pub struct Undelivered {
    /// The run.
    pub run_id: String,
    /// The target.
    pub target: String,
    /// `pending`, `sending`, `failed` or `unknown`.
    pub state: String,
    /// The last failure.
    pub error: Option<String>,
}

/// One setting.
#[derive(Debug, Clone, Serialize)]
pub struct Setting {
    /// The key.
    pub key: String,
    /// Its effective value (JSON).
    pub value: serde_json::Value,
    /// Where it came from.
    pub source: String,
}

/// The tick lease holder.
#[derive(Debug, Clone, Serialize)]
pub struct Holder {
    /// `daemon` or `tick`.
    pub kind: String,
    /// Its process.
    pub pid: u32,
    /// Its host.
    pub host_id: String,
}

/// One profile's report.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    /// The profile.
    pub profile: String,
    /// The configured trigger.
    pub configured_provider: String,
    /// The trigger in effect.
    pub provider: String,
    /// Why they differ.
    pub fallback: Option<String>,
    /// Who holds the tick lease.
    pub lease: Option<Holder>,
    /// Enabled jobs whose next run is past and not fired.
    pub due_unfired: usize,
    /// Jobs in `error` or whose last run was `blocked_config`.
    pub problems: Vec<Problem>,
    /// Outbox rows not delivered.
    pub undelivered: Vec<Undelivered>,
    /// The `cron.*` settings.
    pub settings: Vec<Setting>,
}

impl Report {
    /// Exit 1 when a job is in `error` or an outbox row is `unknown`.
    #[must_use]
    pub fn healthy(&self) -> bool {
        !self
            .problems
            .iter()
            .any(|p| p.kind == JobState::Error.as_str())
            && !self.undelivered.iter().any(|u| u.state == "unknown")
    }
}

/// The report of `scope` over `store` at `now`.
///
/// # Errors
///
/// Store errors.
pub fn report(scope: &ProfileScope, store: &Store, now: Timestamp) -> Result<Report, Error> {
    let config = &scope.config().cron;
    let (provider, fallback) = effective_provider(config);
    let jobs = store.cron_jobs()?;
    let now_ms = now.as_millisecond();
    let due_unfired = jobs
        .iter()
        .filter(|j| {
            j.enabled
                && j.state == JobState::Scheduled.as_str()
                && j.next_run_at.is_some_and(|at| at <= now_ms)
        })
        .count();
    let problems = jobs
        .iter()
        .filter_map(|j| {
            let kind = if j.state == JobState::Error.as_str() {
                JobState::Error.as_str()
            } else if j.last_status.as_deref() == Some(Status::BlockedConfig.as_str()) {
                Status::BlockedConfig.as_str()
            } else {
                return None;
            };
            Some(Problem {
                id: j.id.clone(),
                name: j.name.clone(),
                kind: kind.to_owned(),
                detail: j.last_error.clone(),
            })
        })
        .collect();
    let undelivered = store
        .cron_undelivered()?
        .into_iter()
        .map(|d| Undelivered {
            run_id: d.run_id,
            target: d.target,
            state: d.state,
            error: d.error,
        })
        .collect();
    let lease = store.cron_lease_holder()?.map(|h| Holder {
        kind: h.kind,
        pid: h.holder.pid,
        host_id: h.holder.host_id,
    });
    let layers = scope.layers();
    let settings = SETTINGS
        .iter()
        .map(|key| Setting {
            key: (*key).to_owned(),
            value: layers
                .value(key)
                .cloned()
                .unwrap_or(serde_json::Value::Null),
            source: layers.source(key).to_string(),
        })
        .collect();
    Ok(Report {
        profile: scope.name().as_str().to_owned(),
        configured_provider: config.provider.as_str().to_owned(),
        provider: provider.as_str().to_owned(),
        fallback,
        lease,
        due_unfired,
        problems,
        undelivered,
        settings,
    })
}

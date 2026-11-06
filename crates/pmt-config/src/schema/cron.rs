//! The `[cron]` section (spec: scheduled-jobs). Every scheduler knob is a
//! typed key here; none is read from the environment.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Which trigger signals due occurrences (spec: scheduled-jobs, Pluggable
/// trigger provider).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CronProvider {
    /// The ticker inside the gateway daemon (or `prometheus cron tick`).
    #[default]
    Builtin,
    /// An external managed scheduler; falls back to `builtin` with a
    /// warning while it is unavailable.
    Managed,
}

impl CronProvider {
    /// The configured spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::Managed => "managed",
        }
    }
}

/// The `[cron]` section: the profile time zone (also the zone of daily
/// budgets and `usage --by day`) and the scheduled-job settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "live"))]
#[allow(clippy::struct_excessive_bools)] // independent switches of the spec
pub struct CronConfig {
    /// IANA time zone of this profile (`Europe/Lisbon`); unset uses the
    /// host's zone.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    /// The trigger: `builtin` or `managed`.
    pub provider: CronProvider,
    /// Seconds between the built-in trigger's evaluations of due jobs (it
    /// also wakes at the earliest next run).
    #[schemars(range(min = 10, max = 300))]
    pub tick_interval_secs: u64,
    /// Collapse missed occurrences of a job without an explicit `grace`
    /// into one catch-up run (otherwise they are recorded `skipped_late`).
    pub catch_up_missed: bool,
    /// Runs executing at once per profile (1 = serial).
    #[schemars(range(min = 1, max = 64))]
    pub max_parallel_jobs: u32,
    /// Seconds an agent run may go without tokens or tool activity before
    /// it is interrupted (`0` = unlimited); a job's `timeout.idle` wins.
    pub agent_idle_timeout_secs: u64,
    /// Seconds a run may take in all (`0` = unlimited); a job's
    /// `timeout.wall` wins.
    pub max_run_secs: u64,
    /// Seconds a job script may run before it is killed.
    #[schemars(range(min = 1, max = 86_400))]
    pub script_timeout_secs: u64,
    /// Toolsets of runs whose job names none; unset uses the cron
    /// surface's default (`toolsets.surfaces.cron`, else the enabled
    /// toolsets).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_toolsets: Option<Vec<String>>,
    /// Offer the `cronjob` tool inside scheduled runs.
    pub allow_agent_scheduling: bool,
    /// Check a job's credential, skills and delivery adapters before each
    /// run; a failure is recorded `blocked_config` without a model call.
    pub preflight: bool,
    /// Frame delivered output with the job name and a pause hint.
    pub wrap_response: bool,
    /// Seconds run records are kept (`0` = forever).
    pub run_retention_secs: u64,
    /// Seconds completed jobs are kept before deletion (`0` = never
    /// deleted).
    pub completed_retention_secs: u64,
}

impl Default for CronConfig {
    fn default() -> Self {
        Self {
            timezone: None,
            provider: CronProvider::Builtin,
            tick_interval_secs: 30,
            catch_up_missed: true,
            max_parallel_jobs: 4,
            agent_idle_timeout_secs: 600,
            max_run_secs: 3600,
            script_timeout_secs: 120,
            default_toolsets: None,
            allow_agent_scheduling: false,
            preflight: true,
            wrap_response: true,
            run_retention_secs: 7_776_000,
            completed_retention_secs: 604_800,
        }
    }
}

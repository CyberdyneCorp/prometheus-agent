//! One run, from preflight to delivery (spec: scheduled-jobs, Pre-dispatch
//! preflight; Script-backed and no-agent jobs; Prompt and payload safety
//! scan; Response framing and silence; Delivery targets; Repeat limits;
//! and the M2 upgrades: wake gate, monitor mode, chaining, notepad, run
//! context, failure handling).
//!
//! What a run executes is the binary's: a [`Runner`] starts the agent
//! session (fresh, `source = cron`, its own tools, memory policy and
//! fail-closed approvals), runs scripts in the sandbox and fetches
//! monitored URLs. This module owns the order of the stages ([`stages`])
//! and what is recorded afterwards ([`after`]).

mod after;
mod stages;

pub use stages::{NO_CHANGE, WAKE_AGENT_FALSE};

use std::path::Path;
use std::time::Duration;

use jiff::Timestamp;
use jiff::tz::TimeZone;
use pmt_agent::turn::model::BoxFuture;
use pmt_config::schema::CronConfig;
use pmt_core::error::Error;
use pmt_store::Store;
use pmt_store::cron::RunRow;
use pmt_store::leases::Holder;
use serde_json::json;

use crate::job::{Job, Status};

/// Script stdout injected into a prompt (or delivered) at most.
pub const MAX_SCRIPT_STDOUT: usize = 64 * 1024;
/// The stderr tail kept for a failed script.
pub const STDERR_TAIL: usize = 4 * 1024;
/// Outputs larger than this go to the blob store.
pub const MAX_INLINE_OUTPUT: usize = 64 * 1024;
/// A monitored URL's body read at most.
pub const MAX_MONITOR_BODY: usize = 1024 * 1024;
/// How long a monitored URL may take.
pub const MONITOR_TIMEOUT: Duration = Duration::from_secs(30);

/// What a script did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScriptOutcome {
    /// Its exit code (`None` when killed).
    pub exit_code: Option<i32>,
    /// Its standard output.
    pub stdout: String,
    /// Its standard error.
    pub stderr: String,
    /// It ran past its time limit and was killed.
    pub timed_out: bool,
}

/// One agent run to start.
#[derive(Debug, Clone)]
pub struct AgentRequest {
    /// The job.
    pub job: Job,
    /// The run id.
    pub run_id: String,
    /// The assembled prompt (script output included).
    pub prompt: String,
    /// Interrupt after this long without tokens or tool activity.
    pub idle: Option<Duration>,
    /// Interrupt after this long in all.
    pub wall: Option<Duration>,
    /// A `cron replay --live` re-run (its session is tagged `cron_replay`).
    pub replay: bool,
}

/// How an agent run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentEnd {
    /// The final answer.
    Completed(String),
    /// The run failed (the message says why).
    Failed(String),
    /// A time limit interrupted it: `idle` or `wall`.
    TimedOut(&'static str),
    /// A transient network or DNS error before any model request was
    /// sent (re-run automatically).
    Unreachable(String),
    /// The provider's subscription usage window is closed until this
    /// instant (Unix ms) and no fallback is usable (the job is held).
    UsageClosed(i64),
}

/// What an agent run produced.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentOutcome {
    /// Its session.
    pub session_id: Option<String>,
    /// How it ended.
    pub end: AgentEnd,
    /// Input tokens from the usage ledger.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Cost in USD.
    pub cost_usd: f64,
    /// A model request reached the provider.
    pub reached_model: bool,
}

/// What executes a run's stages.
pub trait Runner: Send + Sync {
    /// Checks the job's credential, skills and delivery adapters; `Err`
    /// names the missing item.
    fn preflight<'a>(&'a self, job: &'a Job) -> BoxFuture<'a, Result<(), String>>;
    /// The bodies of the job's skills, for the run-time safety scan.
    fn skill_text(&self, job: &Job) -> String;
    /// Runs `script` (inside the profile's scripts directory) under the
    /// sandbox with the job's grants, interpreter and script secrets.
    fn run_script<'a>(
        &'a self,
        job: &'a Job,
        script: &'a Path,
        timeout: Duration,
    ) -> BoxFuture<'a, ScriptOutcome>;
    /// Fetches a monitored URL: a bounded GET through the egress proxy
    /// ([`MONITOR_TIMEOUT`], [`MAX_MONITOR_BODY`]); `Err` says why not.
    fn fetch<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Result<String, String>>;
    /// Runs the agent.
    fn run_agent(&self, request: AgentRequest) -> BoxFuture<'_, AgentOutcome>;
    /// Redacts known secrets from delivered text.
    fn redact(&self, text: &str) -> String;
}

/// What [`execute`] works with.
pub struct Exec<'a> {
    /// The profile's store.
    pub store: &'a Store,
    /// The profile's name (the own Bot Chat).
    pub profile: &'a str,
    /// The profile zone.
    pub zone: &'a TimeZone,
    /// `<profile>/scripts`.
    pub scripts_dir: &'a Path,
    /// The `[cron]` settings.
    pub config: &'a CronConfig,
    /// The stages.
    pub runner: &'a dyn Runner,
    /// Where run and delivery times come from.
    pub clock: &'a dyn crate::clock::Clock,
}

impl Exec<'_> {
    fn now(&self) -> Timestamp {
        self.clock.now()
    }

    fn now_ms(&self) -> i64 {
        self.now().as_millisecond()
    }

    fn limits(&self, job: &Job) -> (Option<Duration>, Option<Duration>) {
        let timeout = &job.spec.timeout;
        (
            secs(
                timeout
                    .idle_secs
                    .unwrap_or(self.config.agent_idle_timeout_secs),
            ),
            secs(timeout.wall_secs.unwrap_or(self.config.max_run_secs)),
        )
    }
}

/// The result of the stages before delivery.
#[derive(Debug, Default)]
struct Produced {
    status: Option<Status>,
    reason: Option<String>,
    error: Option<String>,
    output: Option<String>,
    /// Content to deliver (absent: nothing to send).
    deliver: Option<String>,
    agent: Option<AgentOutcome>,
    executed: bool,
    inputs: serde_json::Value,
    /// A model request reached the provider.
    reached_model: bool,
    /// The model could not be reached (transient network or DNS).
    unreachable: bool,
    /// The usage window is closed until this instant.
    hold_until: Option<i64>,
    /// The monitor's new hash and output.
    monitor: Option<(String, String)>,
}

impl Produced {
    fn ended(status: Status, executed: bool) -> Self {
        Self {
            status: Some(status),
            executed,
            inputs: json!({}),
            ..Self::default()
        }
    }

    fn failed(status: Status, error: String, executed: bool) -> Self {
        Self {
            error: Some(error),
            ..Self::ended(status, executed)
        }
    }

    fn silent(reason: &str) -> Self {
        Self {
            reason: Some(reason.to_owned()),
            ..Self::ended(Status::Silent, true)
        }
    }

    fn status(&self) -> Status {
        self.status.unwrap_or(Status::Failed)
    }
}

fn secs(value: u64) -> Option<Duration> {
    (value > 0).then(|| Duration::from_secs(value))
}

fn tail(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_owned()
}

fn head(text: &str, max: usize) -> String {
    crate::incident::truncate(text, max)
}

/// Moves `run` to `running` under this process; `None` when another
/// executor took a queued run first.
fn start(store: &Store, run: &RunRow, now_ms: i64) -> Result<Option<RunRow>, Error> {
    let mut taken = true;
    let started = store.cron_update_run(
        &run.id,
        |r| {
            taken = matches!(r.status.as_str(), "queued" | "running") && r.started_at.is_none();
            if taken {
                "running".clone_into(&mut r.status);
                r.started_at = Some(now_ms);
                r.owner = Some(Holder::current());
            }
            Ok(())
        },
        |_, _| Ok(()),
    )?;
    Ok(taken.then_some(started))
}

/// Executes `run` of `job`; returns the finished run record.
///
/// # Errors
///
/// Store errors (a failure of the run itself is recorded, not returned).
pub async fn execute(ctx: &Exec<'_>, job: &Job, run: &RunRow) -> Result<RunRow, Error> {
    let Some(run) = start(ctx.store, run, ctx.now_ms())? else {
        return Ok(run.clone());
    };
    if ctx.config.preflight
        && let Err(missing) = ctx.runner.preflight(job).await
    {
        return after::blocked(ctx, job, &run, &missing);
    }
    if job.row.alerted {
        ctx.store.cron_modify_job(job.id(), |row| {
            row.alerted = false;
            Ok(())
        })?;
    }
    let produced = stages::produce(ctx, job, &run).await;
    after::finish(ctx, job, &run, produced)
}

/// `cron replay --live` (spec: scheduled-jobs, Run replay): re-runs the
/// recorded inputs of `original` against the current provider and
/// harness as a new `replay` run of `job`; delivered only with `deliver`.
///
/// # Errors
///
/// `cron.invalid_state` for a run without recorded agent inputs, store
/// errors.
pub async fn replay(
    ctx: &Exec<'_>,
    job: &Job,
    original: &RunRow,
    deliver: bool,
) -> Result<RunRow, Error> {
    let inputs: serde_json::Value = original
        .inputs
        .as_deref()
        .and_then(|text| serde_json::from_str(text).ok())
        .unwrap_or_default();
    let Some(prompt) = inputs["prompt"].as_str() else {
        return Err(Error::new(
            &pmt_core::error::codes::CRON_INVALID_STATE,
            format!("run {} has no recorded agent inputs to replay", original.id),
        ));
    };
    let mut run = crate::scheduler::run_row(job, ctx.now(), "replay", Status::Ok);
    "running".clone_into(&mut run.status);
    run.started_at = Some(ctx.now_ms());
    run.owner = Some(Holder::current());
    ctx.store.cron_insert_run(&run)?;
    let (idle, wall) = ctx.limits(job);
    let request = AgentRequest {
        job: job.clone(),
        run_id: run.id.clone(),
        prompt: prompt.to_owned(),
        idle,
        wall,
        replay: true,
    };
    let outcome = ctx.runner.run_agent(request).await;
    let mut produced = stages::agent_result(outcome, inputs);
    if !deliver {
        produced.deliver = None;
    }
    after::finish(ctx, job, &run, produced)
}

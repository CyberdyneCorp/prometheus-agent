//! One run, from preflight to delivery (spec: scheduled-jobs, Pre-dispatch
//! preflight; Script-backed and no-agent jobs; Prompt and payload safety
//! scan; Response framing and silence; Delivery targets; Repeat limits).
//!
//! What a run executes is the binary's: a [`Runner`] starts the agent
//! session (fresh, `source = cron`, its own tools, memory policy and
//! fail-closed approvals) and runs scripts in the sandbox. This module owns
//! the order of the stages and what is recorded.

use std::path::Path;
use std::time::Duration;

use jiff::Timestamp;
use pmt_agent::turn::model::BoxFuture;
use pmt_config::schema::CronConfig;
use pmt_core::error::Error;
use pmt_store::Store;
use pmt_store::cron::{JobRow, RunRow};
use pmt_store::leases::Holder;
use serde_json::json;

use crate::deliver;
use crate::job::{Job, JobState, Status};
use crate::scan;
use crate::scheduler::complete;
use crate::target::{Resolved, Target, resolve};
use crate::validate;

/// Script stdout injected into a prompt (or delivered) at most.
pub const MAX_SCRIPT_STDOUT: usize = 64 * 1024;
/// The stderr tail kept for a failed script.
pub const STDERR_TAIL: usize = 4 * 1024;
/// Outputs larger than this go to the blob store.
pub const MAX_INLINE_OUTPUT: usize = 64 * 1024;

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
}

/// What executes a run's stages.
pub trait Runner: Send + Sync {
    /// Checks the job's credential, skills and delivery adapters; `Err`
    /// names the missing item.
    fn preflight<'a>(&'a self, job: &'a Job) -> BoxFuture<'a, Result<(), String>>;
    /// The bodies of the job's skills, for the run-time safety scan.
    fn skill_text(&self, job: &Job) -> String;
    /// Runs `script` (inside the profile's scripts directory) under the
    /// sandbox with the job's grants.
    fn run_script<'a>(
        &'a self,
        job: &'a Job,
        script: &'a Path,
        timeout: Duration,
    ) -> BoxFuture<'a, ScriptOutcome>;
    /// Runs the agent.
    fn run_agent(&self, request: AgentRequest) -> BoxFuture<'_, AgentOutcome>;
    /// Redacts known secrets from delivered text.
    fn redact(&self, text: &str) -> String;
}

/// What [`execute`] works with.
pub struct Exec<'a> {
    /// The profile's store.
    pub store: &'a Store,
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
}

/// The result of the stages before delivery.
struct Produced {
    status: Status,
    reason: Option<String>,
    error: Option<String>,
    output: Option<String>,
    /// Content to deliver (absent: nothing to send).
    deliver: Option<String>,
    agent: Option<AgentOutcome>,
    executed: bool,
    inputs: serde_json::Value,
}

impl Produced {
    fn failed(status: Status, error: String, executed: bool) -> Self {
        Self {
            status,
            reason: None,
            error: Some(error),
            output: None,
            deliver: None,
            agent: None,
            executed,
            inputs: json!({}),
        }
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
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
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
        return blocked(ctx, job, &run, &missing);
    }
    if job.row.alerted {
        ctx.store.cron_modify_job(job.id(), |row| {
            row.alerted = false;
            Ok(())
        })?;
    }
    let produced = produce(ctx, job, &run).await;
    finish(ctx, job, &run, produced)
}

/// `blocked_config`: no model call, one alert until a preflight passes.
fn blocked(ctx: &Exec<'_>, job: &Job, run: &RunRow, missing: &str) -> Result<RunRow, Error> {
    tracing::warn!(job = job.id(), missing, "cron job blocked by its preflight");
    if !job.row.alerted {
        let body = format!(
            "⚠ cron job {} ({}) is blocked: {missing}",
            job.row.name,
            job.id()
        );
        let rows: Vec<_> = messaging_targets(job)
            .iter()
            .map(|t| deliver::row(run, t, &body, true, ctx.now_ms()))
            .collect();
        ctx.store.cron_enqueue(&rows)?;
    }
    let mut produced = Produced::failed(Status::BlockedConfig, missing.to_owned(), false);
    produced.reason = Some(missing.to_owned());
    let finished = finish(ctx, job, run, produced)?;
    ctx.store.cron_modify_job(job.id(), |row| {
        row.alerted = true;
        Ok(())
    })?;
    Ok(finished)
}

/// The job's messaging destinations.
fn messaging_targets(job: &Job) -> Vec<String> {
    job.spec
        .deliver
        .iter()
        .filter_map(|t| Target::parse(t).ok())
        .filter_map(|t| match resolve(&t, job.origin.as_ref()) {
            Resolved::Messaging(target) => Some(target),
            _ => None,
        })
        .collect()
}

/// Runs the script and agent stages.
async fn produce(ctx: &Exec<'_>, job: &Job, run: &RunRow) -> Produced {
    let mut script_output = None;
    if let Some(script) = &job.spec.script {
        match run_script(ctx, job, script).await {
            Ok(stdout) => script_output = Some(stdout),
            Err(produced) => return produced,
        }
    }
    if job.spec.no_agent {
        let stdout = script_output.unwrap_or_default();
        let empty = stdout.trim().is_empty();
        return Produced {
            status: if empty { Status::Silent } else { Status::Ok },
            reason: None,
            error: None,
            output: Some(stdout.clone()),
            deliver: (!empty).then_some(stdout),
            agent: None,
            executed: true,
            inputs: json!({"script": job.spec.script}),
        };
    }
    let prompt = assemble(&job.spec.prompt, script_output.as_deref());
    let scanned = format!("{prompt}\n{}", ctx.runner.skill_text(job));
    if let Some(rule) = scan::violation(&scanned) {
        return Produced::failed(Status::Failed, format!("unsafe prompt: {rule}"), true);
    }
    let inputs =
        json!({"prompt": prompt, "skills": job.spec.skills, "script_output": script_output});
    let request = AgentRequest {
        job: job.clone(),
        run_id: run.id.clone(),
        prompt,
        idle: secs(
            job.spec
                .timeout
                .idle_secs
                .unwrap_or(ctx.config.agent_idle_timeout_secs),
        ),
        wall: secs(
            job.spec
                .timeout
                .wall_secs
                .unwrap_or(ctx.config.max_run_secs),
        ),
        replay: false,
    };
    let outcome = ctx.runner.run_agent(request).await;
    agent_result(outcome, inputs)
}

fn agent_result(outcome: AgentOutcome, inputs: serde_json::Value) -> Produced {
    let mut produced = match &outcome.end {
        AgentEnd::Completed(text) if deliver::is_silent(text) => Produced {
            status: Status::Silent,
            reason: None,
            error: None,
            output: Some(text.clone()),
            deliver: None,
            agent: None,
            executed: true,
            inputs: json!({}),
        },
        AgentEnd::Completed(text) => Produced {
            status: Status::Ok,
            reason: None,
            error: None,
            output: Some(text.clone()),
            deliver: Some(text.clone()),
            agent: None,
            executed: true,
            inputs: json!({}),
        },
        AgentEnd::Failed(error) => Produced::failed(Status::Failed, error.clone(), true),
        AgentEnd::TimedOut(reason) => {
            let mut p = Produced::failed(
                Status::TimedOut,
                format!("interrupted after the {reason} timeout"),
                true,
            );
            p.reason = Some((*reason).to_owned());
            p
        }
    };
    produced.inputs = inputs;
    produced.agent = Some(outcome);
    produced
}

/// The prompt with the script's output appended.
fn assemble(prompt: &str, script_output: Option<&str>) -> String {
    match script_output {
        Some(stdout) => format!(
            "{}\n\n## Script output\n\n{}",
            prompt.trim_end(),
            head(stdout, MAX_SCRIPT_STDOUT)
        ),
        None => prompt.to_owned(),
    }
}

async fn run_script(ctx: &Exec<'_>, job: &Job, script: &str) -> Result<String, Produced> {
    let path = validate::script_path(ctx.scripts_dir, script)
        .map_err(|e| Produced::failed(Status::Failed, e.message().to_owned(), false))?;
    let timeout = Duration::from_secs(ctx.config.script_timeout_secs.max(1));
    let outcome = ctx.runner.run_script(job, &path, timeout).await;
    if outcome.timed_out {
        let mut produced = Produced::failed(
            Status::TimedOut,
            format!("script ran past {} s", timeout.as_secs()),
            true,
        );
        produced.reason = Some("script".to_owned());
        return Err(produced);
    }
    if outcome.exit_code != Some(0) {
        let code = outcome
            .exit_code
            .map_or_else(|| "a signal".to_owned(), |c| format!("exit {c}"));
        let stderr = tail(&outcome.stderr, STDERR_TAIL);
        return Err(Produced::failed(
            Status::Failed,
            format!("script failed ({code}): {}", stderr.trim_end()),
            true,
        ));
    }
    Ok(head(&outcome.stdout, MAX_SCRIPT_STDOUT))
}

/// Queues the deliveries of `content`; the status the run ends with.
fn queue_deliveries(
    ctx: &Exec<'_>,
    job: &Job,
    run: &RunRow,
    content: &str,
) -> Result<(Status, Option<String>), Error> {
    // A no-agent job's output is delivered verbatim.
    let wrap = ctx.config.wrap_response && !job.spec.no_agent;
    let body = ctx
        .runner
        .redact(&deliver::frame(job.id(), &job.row.name, content, wrap));
    let mut rows = Vec::new();
    let mut failure = None;
    for target in &job.spec.deliver {
        let Ok(target) = Target::parse(target) else {
            continue;
        };
        match resolve(&target, job.origin.as_ref()) {
            Resolved::Stored => {}
            Resolved::Messaging(to) => {
                rows.push(deliver::row(run, &to, &body, false, ctx.now_ms()));
            }
            Resolved::BotChat(_) => {
                failure = Some("bot_chat delivery is not available in this build".to_owned());
            }
        }
    }
    ctx.store.cron_enqueue(&rows)?;
    Ok(match (failure, rows.is_empty()) {
        (Some(error), _) => (Status::DeliveryFailed, Some(error)),
        (None, true) => (Status::Ok, None),
        (None, false) => (Status::DeliveryQueued, None),
    })
}

/// Stores the output (inline or as a blob) and settles the run and job.
fn finish(
    ctx: &Exec<'_>,
    job: &Job,
    run: &RunRow,
    mut produced: Produced,
) -> Result<RunRow, Error> {
    if let Some(content) = produced.deliver.take() {
        let (status, error) = queue_deliveries(ctx, job, run, &content)?;
        produced.status = status;
        if error.is_some() {
            produced.error = error;
        }
    }
    let (output, blob) = store_output(ctx.store, &run.id, produced.output.take())?;
    let now = ctx.now();
    let finished = ctx.store.cron_update_run(
        &run.id,
        |r| {
            produced.status.as_str().clone_into(&mut r.status);
            r.reason.clone_from(&produced.reason);
            r.error.clone_from(&produced.error);
            r.ended_at = Some(now.as_millisecond());
            r.output.clone_from(&output);
            r.output_blob.clone_from(&blob);
            r.inputs = Some(produced.inputs.to_string());
            if let Some(agent) = &produced.agent {
                r.session_id.clone_from(&agent.session_id);
                r.input_tokens = agent.input_tokens;
                r.output_tokens = agent.output_tokens;
                r.cost_usd = agent.cost_usd;
            }
            Ok(())
        },
        |r, row| {
            settle_job(row, job, r, produced.executed, now);
            Ok(())
        },
    )?;
    if produced.status == Status::DeliveryQueued {
        // Rows may have settled already (a fast sender): write back again.
        deliver::write_back(ctx.store, &run.id)?;
    }
    Ok(finished)
}

fn store_output(
    store: &Store,
    run_id: &str,
    output: Option<String>,
) -> Result<(Option<String>, Option<String>), Error> {
    match output {
        Some(text) if text.len() > MAX_INLINE_OUTPUT => {
            let blob = store.put_blob(text.as_bytes())?;
            store.reference_blob(&blob, "cron_run", run_id)?;
            Ok((
                Some(head(&text, MAX_INLINE_OUTPUT)),
                Some(blob.sha256().to_owned()),
            ))
        }
        other => Ok((other, None)),
    }
}

/// The job after a run: its last status, the repeat counter for an
/// executed scheduled occurrence, and its state.
fn settle_job(row: &mut JobRow, job: &Job, run: &RunRow, executed: bool, now: Timestamp) {
    if run.trigger == "replay" {
        return;
    }
    row.last_status = Some(run.status.clone());
    row.last_error.clone_from(&run.error);
    let scheduled = run.trigger == "schedule";
    if executed && scheduled {
        row.completed = row.completed.saturating_add(1);
    }
    if row.state != JobState::Running.as_str() || !scheduled {
        return;
    }
    let limit_reached = job
        .spec
        .repeat_limit()
        .is_some_and(|limit| row.completed >= limit);
    if limit_reached {
        complete(row, now);
    } else if row.next_run_at.is_none() && job.spec.schedule.recurring() {
        JobState::Error.as_str().clone_into(&mut row.state);
    } else if row.next_run_at.is_none() {
        complete(row, now);
    } else {
        JobState::Scheduled.as_str().clone_into(&mut row.state);
    }
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
    let request = AgentRequest {
        job: job.clone(),
        run_id: run.id.clone(),
        prompt: prompt.to_owned(),
        idle: secs(
            job.spec
                .timeout
                .idle_secs
                .unwrap_or(ctx.config.agent_idle_timeout_secs),
        ),
        wall: secs(
            job.spec
                .timeout
                .wall_secs
                .unwrap_or(ctx.config.max_run_secs),
        ),
        replay: true,
    };
    let outcome = ctx.runner.run_agent(request).await;
    let mut produced = agent_result(outcome, inputs);
    if !deliver {
        produced.deliver = None;
    }
    finish(ctx, job, &run, produced)
}

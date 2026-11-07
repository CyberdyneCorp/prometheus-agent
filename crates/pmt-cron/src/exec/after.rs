//! What is recorded after a run's stages: deliveries, the failure notice
//! (routing, incidents, the review nudge), the re-run ladder and usage
//! window hold, the output, and the run and job records.

use jiff::Timestamp;
use pmt_core::error::Error;
use pmt_store::Store;
use pmt_store::cron::{DeliveryRow, JobRow, RunRow};

use super::{Exec, MAX_INLINE_OUTPUT, Produced, head};
use crate::job::{Job, JobState, Status};
use crate::scheduler::complete;
use crate::target::{Resolved, Target, resolve};
use crate::{deliver, failure, incident};

/// `blocked_config`: no model call, one alert until a preflight passes.
pub(super) fn blocked(
    ctx: &Exec<'_>,
    job: &Job,
    run: &RunRow,
    missing: &str,
) -> Result<RunRow, Error> {
    tracing::warn!(job = job.id(), missing, "cron job blocked by its preflight");
    if !job.row.alerted {
        let body = format!(
            "⚠ cron job {} ({}) is blocked: {missing}",
            job.row.name,
            job.id()
        );
        let rows = notice_rows(ctx, job, run, &body);
        ctx.store.cron_enqueue(&rows)?;
    }
    let produced = Produced {
        reason: Some(missing.to_owned()),
        ..Produced::failed(Status::BlockedConfig, missing.to_owned(), false)
    };
    let finished = finish(ctx, job, run, produced)?;
    ctx.store.cron_modify_job(job.id(), |row| {
        row.alerted = true;
        Ok(())
    })?;
    Ok(finished)
}

/// The targets failure notices go to: `failure_deliver`, else `deliver`.
fn failure_targets(job: &Job) -> &[String] {
    if job.spec.failure_deliver.is_empty() {
        &job.spec.deliver
    } else {
        &job.spec.failure_deliver
    }
}

/// Queued notice rows of `body` for the job's failure targets (messaging
/// destinations only; `local` records state and sends nothing).
fn notice_rows(ctx: &Exec<'_>, job: &Job, run: &RunRow, body: &str) -> Vec<DeliveryRow> {
    let body = ctx.runner.redact(body);
    let warning = !ctx.config.delivery.notify;
    failure_targets(job)
        .iter()
        .filter_map(|t| Target::parse(t).ok())
        .filter_map(|t| match resolve(&t, job.origin.as_ref()) {
            Resolved::Messaging(to) => {
                let to = ctx.thread_of(&t, to);
                Some(deliver::row(run, &to, &body, warning, ctx.now_ms()))
            }
            _ => None,
        })
        .collect()
}

impl Exec<'_> {
    /// A home-chat target with its configured cron thread
    /// (`cron.delivery.threads.<platform>`); explicit chats keep theirs.
    fn thread_of(&self, target: &Target, resolved: String) -> String {
        match target {
            Target::Home(platform) => match self.config.delivery.threads.get(platform) {
                Some(thread) => format!("{resolved}::{thread}"),
                None => resolved,
            },
            _ => resolved,
        }
    }
}

/// Whether a delivery of `job` to `target` opens a conversation (spec:
/// Continuable deliveries): the job's `attach_to_session`, else
/// `cron.continuable`, for its origin chat, a home-chat fallback, or a
/// single explicit target of a job that asked for it.
fn continuable(ctx: &Exec<'_>, job: &Job, target: &Target) -> bool {
    let attach = job.spec.attach_to_session.unwrap_or(ctx.config.continuable);
    attach
        && match target {
            Target::Origin | Target::Home(_) => true,
            Target::Chat { .. } => {
                job.spec.attach_to_session == Some(true) && job.spec.deliver.len() == 1
            }
            _ => false,
        }
}

/// What queueing a run's deliveries did.
struct Queued {
    status: Status,
    error: Option<String>,
    /// The Bot Chat receipt (its outbox row id).
    receipt: Option<String>,
}

/// Queues the deliveries of `content`; the status the run ends with.
fn queue_deliveries(
    ctx: &Exec<'_>,
    job: &Job,
    run: &RunRow,
    content: &str,
) -> Result<Queued, Error> {
    // A no-agent job's output is delivered verbatim.
    let wrap = ctx.config.wrap_response && !job.spec.no_agent;
    let body = ctx
        .runner
        .redact(&deliver::frame(job.id(), &job.row.name, content, wrap));
    let mut rows = Vec::new();
    for text in &job.spec.deliver {
        let Ok(target) = Target::parse(text) else {
            continue;
        };
        let (to, body) = match resolve(&target, job.origin.as_ref()) {
            Resolved::Stored => continue,
            Resolved::Messaging(to) => (ctx.thread_of(&target, to), body.clone()),
            Resolved::BotChat(profile) => (
                deliver::bot_chat_target(profile.as_deref().unwrap_or(ctx.profile)),
                deliver::bot_chat_body(&job.row.name, &ctx.runner.redact(content)),
            ),
        };
        let mut row = deliver::row(run, &to, &body, false, ctx.now_ms());
        row.continuable = continuable(ctx, job, &target);
        rows.push(row);
    }
    let queued = |status, error: Option<&str>, receipt| Queued {
        status,
        error: error.map(str::to_owned),
        receipt,
    };
    if rows.is_empty() {
        return Ok(queued(Status::Ok, None, None));
    }
    if content.trim().is_empty() {
        // Nothing (no text, no media) is never sent.
        return Ok(queued(Status::DeliveryFailed, Some("empty payload"), None));
    }
    ctx.store.cron_enqueue(&rows)?;
    let receipt = rows
        .iter()
        .find(|r| r.target.starts_with(deliver::BOT_CHAT))
        .map(|r| r.id.clone());
    Ok(queued(Status::DeliveryQueued, None, receipt))
}

/// How the job's re-run ladder changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Retry {
    Keep,
    Clear,
    Schedule { at: i64, step: u32 },
}

/// What follows the run besides its deliveries.
#[derive(Debug, Clone)]
struct Followup {
    streak: u32,
    retry: Retry,
    retry_of: Option<String>,
    hold: Hold,
}

/// How the job's usage-window hold changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hold {
    Keep,
    Clear,
    /// Held until `until`; the next natural occurrence after it.
    Until {
        until: i64,
        next: Option<i64>,
    },
}

/// Plans the ladder and hold, and queues the failure notice.
fn followup(
    ctx: &Exec<'_>,
    job: &Job,
    run: &RunRow,
    produced: &Produced,
    status: Status,
) -> Result<Followup, Error> {
    let recurring = job.spec.schedule.recurring();
    let mut plan = Followup {
        streak: failure::streak(job.row.failure_streak, status),
        retry: Retry::Keep,
        retry_of: None,
        hold: Hold::Keep,
    };
    if produced.reached_model {
        plan.retry = Retry::Clear;
        plan.hold = Hold::Clear;
    }
    let scheduled = matches!(run.trigger.as_str(), "schedule" | "retry");
    if produced.unreachable && recurring && scheduled && ctx.config.retry_unreachable {
        let step = if run.trigger == "retry" {
            job.row.retry_step
        } else {
            0
        };
        if let Some(at) = failure::next_retry(step, ctx.now(), job.row.next_run_at) {
            plan.retry = Retry::Schedule { at, step: step + 1 };
            plan.retry_of = Some(run.retry_of.clone().unwrap_or_else(|| run.id.clone()));
            // The interim failure is not announced.
            return Ok(plan);
        }
        plan.retry = Retry::Clear;
    }
    if let (Some(until), true) = (produced.hold_until, recurring) {
        let hold = failure::hold(job, until, job.row.next_run_at, ctx.zone);
        plan.hold = Hold::Until {
            until,
            next: hold.next,
        };
        if let Some(at) = hold.retry {
            plan.retry = Retry::Schedule { at, step: 0 };
            plan.retry_of = Some(run.id.clone());
        }
        let body = format!(
            "⚠ cron job {} ({}) is held: the provider's usage window is closed until {}; it \
             fires again after it reopens.",
            job.row.name,
            job.id(),
            crate::schedule::at_ms(until)
        );
        ctx.store.cron_enqueue(&notice_rows(ctx, job, run, &body))?;
        return Ok(plan);
    }
    if matches!(status, Status::Failed | Status::TimedOut) {
        failure_notice(ctx, job, run, produced, status, plan.streak)?;
    } else if matches!(status, Status::Ok | Status::Silent) {
        incident::on_success(ctx.store, job.id(), ctx.now_ms());
    }
    Ok(plan)
}

/// Records the incident and, unless it is withheld, queues the notice.
fn failure_notice(
    ctx: &Exec<'_>,
    job: &Job,
    run: &RunRow,
    produced: &Produced,
    status: Status,
    streak: u32,
) -> Result<(), Error> {
    let error = ctx
        .runner
        .redact(produced.error.as_deref().unwrap_or("failed"));
    let announce = incident::on_failure(
        ctx.store,
        job.id(),
        &job.row.name,
        &error,
        ctx.now_ms(),
        ctx.config.failure_repeat_alert_secs,
    );
    if !announce {
        tracing::info!(
            job = job.id(),
            "cron failure notice withheld (incident alerted)"
        );
        return Ok(());
    }
    let threshold = ctx.config.failure_nudge_threshold;
    let body = failure::notice(job, status, &error, streak, threshold);
    ctx.store.cron_enqueue(&notice_rows(ctx, job, run, &body))
}

/// Stores the output (inline or as a blob) and settles the run and job.
pub(super) fn finish(
    ctx: &Exec<'_>,
    job: &Job,
    run: &RunRow,
    mut produced: Produced,
) -> Result<RunRow, Error> {
    let outcome = produced.status();
    let plan = if run.trigger == "replay" {
        None
    } else {
        Some(followup(ctx, job, run, &produced, outcome)?)
    };
    let mut status = outcome;
    let mut receipt = None;
    if let Some(content) = produced.deliver.take() {
        let queued = queue_deliveries(ctx, job, run, &content)?;
        status = queued.status;
        receipt = queued.receipt;
        if queued.error.is_some() {
            produced.error = queued.error;
        }
    }
    let (output, blob) = store_output(ctx.store, &run.id, produced.output.take())?;
    let now = ctx.now();
    let finished = ctx.store.cron_update_run(
        &run.id,
        |r| {
            status.as_str().clone_into(&mut r.status);
            r.reason.clone_from(&produced.reason);
            r.error.clone_from(&produced.error);
            r.ended_at = Some(now.as_millisecond());
            r.output.clone_from(&output);
            r.output_blob.clone_from(&blob);
            r.inputs = Some(produced.inputs.to_string());
            r.receipt.clone_from(&receipt);
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
            if let Some(plan) = &plan {
                apply(row, plan);
            }
            if let Some((hash, output)) = &produced.monitor {
                row.monitor_hash = Some(hash.clone());
                row.monitor_output = Some(output.clone());
            }
            Ok(())
        },
    )?;
    if status == Status::DeliveryQueued {
        // Rows may have settled already (a fast sender): write back again.
        deliver::write_back(ctx.store, &run.id)?;
    }
    Ok(finished)
}

/// Applies the follow-up plan to the job row.
fn apply(row: &mut JobRow, plan: &Followup) {
    row.failure_streak = plan.streak;
    match plan.retry {
        Retry::Keep => {}
        Retry::Clear => {
            row.retry_at = None;
            row.retry_of = None;
            row.retry_step = 0;
        }
        Retry::Schedule { at, step } => {
            row.retry_at = Some(at);
            row.retry_of.clone_from(&plan.retry_of);
            row.retry_step = step;
        }
    }
    match plan.hold {
        Hold::Keep => {}
        Hold::Clear => row.quota_hold_until = None,
        Hold::Until { until, next } => {
            row.quota_hold_until = Some(until);
            if row.state != JobState::Completed.as_str() {
                row.next_run_at = next;
            }
        }
    }
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
    let running = row.state == JobState::Running.as_str();
    if run.trigger == "retry" {
        // A re-run stands in for its occurrence: no repeat count.
        if running {
            JobState::Scheduled.as_str().clone_into(&mut row.state);
        }
        return;
    }
    let scheduled = run.trigger == "schedule";
    if executed && scheduled {
        row.completed = row.completed.saturating_add(1);
    }
    if !running || !scheduled {
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

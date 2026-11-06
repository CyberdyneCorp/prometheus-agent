//! One child's run: its slot, its session, its turn under the stall
//! monitor, the `output_schema` check with one correction, and its
//! result (spec: subagent-delegation, Structured result contract;
//! `output_schema` validation never discards results; Stall detection).

use std::sync::Arc;
use std::time::Duration;

use pmt_agent::turn::{Interrupt, Issuer};
use pmt_obs::trajectory::events::Event;
use pmt_obs::trajectory::payload::{SubagentFinished, SubagentSpawned, Tokens, Usage};
use pmt_obs::trajectory::writer::Emit;
use serde_json::Value;
use tokio::time::Instant;

use super::spawn::{Child, Planned};
use super::tracker::Tracker;
use super::{ChildPlan, ChildSession, Delegator, TurnEnd, TurnOutcome};
use crate::result::{ExitReason, ResultError, ResultUsage, Status, SubagentResult};
use crate::stall::{self, Activity, Policy};
use crate::tree::Entry;

/// What one child runs with.
struct Run<'a> {
    plan: &'a ChildPlan,
    schema: Option<&'a Value>,
    transcript: Option<Arc<crate::transcript::Transcript>>,
}

/// How a run ended before its result is assembled.
struct Ran {
    outcome: TurnOutcome,
    stalled: Option<Duration>,
    schema: SchemaCheck,
}

#[derive(Default)]
struct SchemaCheck {
    output: Option<Value>,
    valid: Option<bool>,
    errors: Vec<String>,
}

impl Delegator {
    pub(super) async fn run_child(self: Arc<Self>, child: Child) -> SubagentResult {
        let Child {
            entry,
            planned,
            admission,
            output_schema,
            transcript,
        } = child;
        let result = match planned {
            Planned::Refused(message) => SubagentResult::refused(
                &entry.subagent_id,
                entry.task_index,
                "delegation.router_refused",
                &message,
            ),
            Planned::Run(plan, goal) => {
                tokio::select! {
                    biased;
                    () = entry.interrupt.token().cancelled() => interrupted(&entry),
                    permit = admission.ready() => {
                        let _permit = permit;
                        let run = Run { plan: &plan, schema: output_schema.as_ref(), transcript: transcript.clone() };
                        self.run_planned(&entry, run, goal).await
                    }
                }
            }
        };
        if let Some(log) = &transcript {
            log.line(&format!(
                "finished: {} {}",
                result.status.as_str(),
                pmt_core::subagent_tree::one_line(&result.summary)
            ));
        }
        self.finish(&entry, result).await
    }

    async fn run_planned(&self, entry: &Arc<Entry>, run: Run<'_>, goal: String) -> SubagentResult {
        let Run {
            plan,
            schema,
            transcript,
        } = run;
        let started = Instant::now();
        let activity = Arc::new(Activity::new());
        let tracker = Tracker::start(
            Arc::clone(entry),
            Arc::clone(&activity),
            self.ctx
                .config
                .progress_events
                .then(|| Arc::clone(&self.ctx.events)),
            transcript.clone(),
        );
        let mut plan = plan.clone();
        plan.events = Arc::new(tracker.clone()) as _;
        let session = match self.ctx.launcher.open(&plan).await {
            Ok(session) => session,
            Err(err) => return not_opened(entry, &err),
        };
        entry.started(&session.id().to_string(), &session.model());
        if let Some(log) = &transcript {
            log.line(&format!(
                "started in session {} on {}",
                session.id(),
                session.model()
            ));
        }
        if let Some(cwd) = session.cwd() {
            tracker.set_cwd(cwd);
        }
        if let Err(err) = self
            .ctx
            .store
            .subagent_started(&entry.subagent_id, session.id())
        {
            tracing::warn!(error = %err, subagent_id = %entry.subagent_id, "subagent start not recorded");
        }
        self.announce(entry, &session, &goal).await;
        let bound = Arc::clone(&session);
        entry.steers.bind(Box::new(move |text| bound.steer(text)));
        let ran = self.drive(entry, &session, goal, schema, &activity).await;
        let missed = entry.steers.close(session.undelivered_steers());
        let closed = session.close().await;
        let mut result = assemble(entry, &session.model(), ran, started);
        result.missed_steer = missed;
        result.orphaned_processes = closed.orphaned_processes;
        result.worktree = closed.worktree;
        result.note = closed.note;
        result.artifacts = tracker.artifacts(&self.ctx.store);
        result.usage.tool_calls = entry.snapshot().tool_calls;
        result.trajectory_ref = Some(session.id().to_string());
        result
    }

    /// The child's turn under the stall monitor, then the schema check.
    async fn drive(
        &self,
        entry: &Entry,
        session: &Arc<dyn ChildSession>,
        goal: String,
        schema: Option<&Value>,
        activity: &Activity,
    ) -> Ran {
        let policy = Policy::new(self.ctx.config.inactivity_timeout_secs);
        let (outcome, stalled) = watched(session, goal, &entry.interrupt, policy, activity).await;
        let mut ran = Ran {
            outcome,
            stalled,
            schema: SchemaCheck::default(),
        };
        if let (Some(schema), None, TurnEnd::FinalAnswer) = (schema, stalled, ran.outcome.end) {
            ran.schema = self
                .check_schema(session, schema, &mut ran.outcome, &entry.interrupt)
                .await;
        }
        ran
    }

    /// Validates the answer, allowing one correction turn.
    async fn check_schema(
        &self,
        session: &Arc<dyn ChildSession>,
        schema: &Value,
        outcome: &mut TurnOutcome,
        interrupt: &Interrupt,
    ) -> SchemaCheck {
        let answer = outcome.text.clone().unwrap_or_default();
        let errors = match crate::output::check(schema, &answer) {
            Ok(value) => return valid(value),
            Err(errors) => errors,
        };
        let retry = session
            .turn(crate::output::correction(&errors), interrupt.clone())
            .await;
        merge_usage(outcome, &retry);
        if retry.end == TurnEnd::FinalAnswer {
            let corrected = retry.text.clone().unwrap_or_default();
            match crate::output::check(schema, &corrected) {
                Ok(value) => {
                    outcome.text = Some(corrected);
                    return valid(value);
                }
                Err(errors) => {
                    return SchemaCheck {
                        output: None,
                        valid: Some(false),
                        errors,
                    };
                }
            }
        }
        SchemaCheck {
            output: None,
            valid: Some(false),
            errors,
        }
    }

    async fn announce(&self, entry: &Entry, session: &Arc<dyn ChildSession>, goal: &str) {
        if !self.ctx.config.progress_events {
            return;
        }
        let event = Event::SubagentSpawned(SubagentSpawned {
            subagent_id: entry.subagent_id.clone(),
            child_session_id: session.id().to_string(),
            depth: entry.depth,
            goal: pmt_core::subagent_tree::one_line(goal),
        });
        if let Err(err) = self.ctx.events.emit(Emit::now(event)).await {
            tracing::debug!(error = %err, "subagent.spawned not recorded");
        }
    }

    /// Records the final result everywhere it belongs.
    async fn finish(&self, entry: &Entry, result: SubagentResult) -> SubagentResult {
        if self.suspended() {
            // The process is stopping and took the child with it: its row
            // stays open for the next start to finalise as interrupted.
            return result;
        }
        let json = serde_json::to_string(&result).unwrap_or_default();
        if let Err(err) =
            self.ctx
                .store
                .subagent_finished(&entry.subagent_id, result.status.as_str(), &json)
        {
            tracing::warn!(error = %err, subagent_id = %entry.subagent_id, "subagent result not recorded");
        }
        entry.finish(&result);
        if self.ctx.config.progress_events {
            let event = Event::SubagentFinished(SubagentFinished {
                subagent_id: entry.subagent_id.clone(),
                status: result.status.as_str().to_owned(),
                summary: pmt_core::subagent_tree::one_line(&result.summary),
                duration_ms: result.duration_ms,
                usage: Usage {
                    tokens: result.usage.tokens,
                    cost_usd: result.cost_usd,
                },
            });
            if let Err(err) = self.ctx.events.emit(Emit::now(event)).await {
                tracing::debug!(error = %err, "subagent.finished not recorded");
            }
        }
        result
    }
}

/// Runs the turn while the stall monitor watches; a stall interrupts it,
/// waits out the grace period, then stops it hard.
async fn watched(
    session: &Arc<dyn ChildSession>,
    goal: String,
    interrupt: &Interrupt,
    policy: Policy,
    activity: &Activity,
) -> (TurnOutcome, Option<Duration>) {
    let turn = session.turn(goal, interrupt.clone());
    tokio::pin!(turn);
    let quiet = tokio::select! {
        outcome = &mut turn => return (outcome, None),
        quiet = stall::watch(policy, activity) => quiet,
    };
    tracing::warn!(
        quiet_secs = quiet.as_secs(),
        "subagent stalled; interrupting it"
    );
    interrupt.stop_soft(Issuer::System("stalled".to_owned()));
    let outcome = if let Ok(outcome) = tokio::time::timeout(stall::GRACE, &mut turn).await {
        outcome
    } else {
        interrupt.stop(Issuer::System("stalled".to_owned()));
        turn.await
    };
    (outcome, Some(quiet))
}

fn valid(value: Value) -> SchemaCheck {
    SchemaCheck {
        output: Some(value),
        valid: Some(true),
        errors: Vec::new(),
    }
}

fn merge_usage(into: &mut TurnOutcome, more: &TurnOutcome) {
    let (a, b) = (&mut into.usage, &more.usage);
    a.calls += b.calls;
    a.input_tokens += b.input_tokens;
    a.output_tokens += b.output_tokens;
    a.cache_read_tokens += b.cache_read_tokens;
    a.cache_write_tokens += b.cache_write_tokens;
    a.reasoning_tokens += b.reasoning_tokens;
    a.cost_usd = a.cost_usd.zip(b.cost_usd).map(|(x, y)| x + y);
    into.iterations += more.iterations;
}

/// The result of a child whose session could not be built: a route,
/// model or profile that does not resolve refuses the task; anything else
/// (a missing credential, an I/O failure) is the child's failure.
fn not_opened(entry: &Entry, err: &pmt_core::Error) -> SubagentResult {
    use pmt_core::error::ErrorKind as K;
    let error = ResultError {
        code: err.code().code.to_owned(),
        message: err.message().to_owned(),
    };
    let status = match err.kind() {
        K::Usage | K::InvalidInput | K::NotFound | K::Config => Status::Refused,
        _ => Status::Failed,
    };
    SubagentResult::ended(
        &entry.subagent_id,
        entry.task_index,
        status,
        ExitReason::Error,
        Some(error),
    )
}

/// The result of a child stopped before it ran.
fn interrupted(entry: &Entry) -> SubagentResult {
    SubagentResult::ended(
        &entry.subagent_id,
        entry.task_index,
        Status::Interrupted,
        ExitReason::Interrupted,
        None,
    )
}

/// Status and exit reason of a run: budget exhaustion with an answer is a
/// truncated completion, not a failure.
fn classify(ran: &Ran, has_text: bool) -> (Status, ExitReason, bool) {
    if ran.stalled.is_some() {
        return (Status::Stalled, ExitReason::Interrupted, false);
    }
    match (ran.outcome.end, has_text) {
        (TurnEnd::FinalAnswer, _) => (Status::Completed, ExitReason::FinalAnswer, false),
        (TurnEnd::MaxIterations, true) => (Status::Completed, ExitReason::MaxIterations, true),
        (TurnEnd::BudgetExhausted, true) => (Status::Completed, ExitReason::BudgetExhausted, true),
        (TurnEnd::MaxIterations, false) => (Status::Failed, ExitReason::MaxIterations, true),
        (TurnEnd::BudgetExhausted, false) => (Status::Failed, ExitReason::BudgetExhausted, true),
        (TurnEnd::Interrupted, _) => (Status::Interrupted, ExitReason::Interrupted, false),
        (TurnEnd::Error, _) => (Status::Failed, ExitReason::Error, false),
    }
}

fn assemble(entry: &Entry, model: &str, ran: Ran, started: Instant) -> SubagentResult {
    let text = ran.outcome.text.clone().unwrap_or_default();
    let (status, exit_reason, truncated) = classify(&ran, !text.trim().is_empty());
    let error = match status {
        Status::Failed => Some(ran.outcome.error.clone().map_or_else(
            || ResultError {
                code: format!("delegation.{}", exit_name(exit_reason)),
                message: "The child ended without an answer.".to_owned(),
            },
            |(code, message)| ResultError { code, message },
        )),
        _ => None,
    };
    let usage = &ran.outcome.usage;
    let mut result = SubagentResult::ended(
        &entry.subagent_id,
        entry.task_index,
        status,
        exit_reason,
        error,
    );
    result.truncated = truncated;
    result.summary = text;
    // Failed children skip validation.
    if status == Status::Completed {
        result.output = ran.schema.output;
        result.schema_valid = ran.schema.valid;
        result.schema_errors = ran.schema.errors;
    }
    result.usage = ResultUsage {
        tokens: Tokens {
            input: usage.input_tokens,
            output: usage.output_tokens,
            cache_read: usage.cache_read_tokens,
            cache_write: usage.cache_write_tokens,
            reasoning: usage.reasoning_tokens,
        },
        iterations: ran.outcome.iterations,
        tool_calls: 0,
    };
    result.cost_usd = usage.cost_usd;
    if usage.cost_usd.is_some() {
        "estimated"
    } else {
        "unknown"
    }
    .clone_into(&mut result.cost_status);
    result.duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    model.clone_into(&mut result.model);
    result.quiet_secs = ran.stalled.map(|q| q.as_secs());
    result
}

fn exit_name(reason: ExitReason) -> &'static str {
    match reason {
        ExitReason::FinalAnswer => "final_answer",
        ExitReason::MaxIterations => "max_iterations",
        ExitReason::BudgetExhausted => "budget_exhausted",
        ExitReason::Interrupted => "interrupted",
        ExitReason::Error => "error",
    }
}

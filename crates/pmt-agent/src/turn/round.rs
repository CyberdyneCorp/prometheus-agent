//! The tool round of an iteration (spec: agent-turn-loop, Iteration
//! phases; Tool loop guardrails; Wall-clock run budget; Step events per
//! iteration).
//!
//! Calls over a per-turn cap are refused without running; the rest run as
//! one planned batch (`tool.batch_planned`). Each result passes the loop
//! guardrails (which may append a warning), the newest one carries the
//! one-time run-budget wrap-up notice when due, results are stored in call
//! order and the iteration ends with `turn.step`.

use pmt_core::error::{Error, codes};
use pmt_obs::trajectory::events::Event;
use pmt_obs::trajectory::payload::{
    StepCall, ToolCall as ToolCallEvent, ToolResult as ToolResultEvent, TurnStep,
};
use pmt_tools::{Content, Status};

use super::exit::ExitReason;
use super::model::{ModelReply, RequestedCall};
use super::run::{Ending, Turn, usage_payload};
use super::tools::ToolOutcome;
use super::transcript;

/// The run-budget wrap-up notice (once per turn, on a tool result).
pub const WRAP_UP_NOTICE: &str = "[prometheus] 80% of this turn's run budget has elapsed. Stop exploring and produce the final deliverable now.";

impl Turn<'_> {
    /// Runs `reply`'s calls; `Some` ends the turn (an interrupt, the run
    /// budget or a guardrail stop).
    pub(super) async fn tool_round(&mut self, reply: &ModelReply) -> Result<Option<Ending>, Error> {
        let calls = &reply.tool_calls;
        for call in calls {
            self.emit(Event::ToolCall(ToolCallEvent {
                call_id: call.id.clone(),
                tool: call.name.clone(),
                arguments: serde_json::from_str(&call.arguments)
                    .unwrap_or_else(|_| serde_json::Value::String(call.arguments.clone())),
            }))
            .await;
        }
        let refusals: Vec<_> = calls.iter().map(|c| self.guard.refusal(c)).collect();
        let runnable: Vec<RequestedCall> = calls
            .iter()
            .zip(&refusals)
            .filter(|(_, refused)| refused.is_none())
            .map(|(call, _)| call.clone())
            .collect();
        let token = self.options.interrupt.token().child();
        let tools = self.agent.setup.tools.clone();
        let batch = match self.race_tools(tools.run(&runnable, &token)).await {
            Ok(batch) => batch,
            Err(end) => {
                token.cancel();
                return Ok(Some(end));
            }
        };
        if let Some(plan) = batch.plan {
            self.emit(Event::ToolBatchPlanned(plan)).await;
        }
        let mut ran = batch.outcomes.into_iter();
        let mut outcomes: Vec<ToolOutcome> = refusals
            .into_iter()
            .map(|refused| match refused {
                Some(result) => ToolOutcome {
                    result,
                    duration_ms: 0,
                },
                None => ran.next().unwrap_or_else(|| ToolOutcome {
                    result: pmt_tools::ToolResult::error("no result"),
                    duration_ms: 0,
                }),
            })
            .collect();
        let stop = self.guard_outcomes(calls, &mut outcomes);
        self.wrap_up(&mut outcomes);
        self.keep_results(calls, &outcomes).await?;
        Ok(stop.map(|why| {
            self.notices.push(why.clone());
            Ending::failed(
                ExitReason::GuardrailStop,
                Error::new(&codes::TURN_GUARDRAIL_STOP, why),
            )
        }))
    }

    /// Runs every result past the guardrails; the first stop wins.
    fn guard_outcomes(
        &mut self,
        calls: &[RequestedCall],
        outcomes: &mut [ToolOutcome],
    ) -> Option<String> {
        let tools = self.agent.setup.tools.clone();
        let mut stop = None;
        for (call, outcome) in calls.iter().zip(outcomes.iter_mut()) {
            let idempotent = tools.idempotent(&call.name);
            let why = self.guard.observe(call, &mut outcome.result, idempotent);
            stop = stop.or(why);
        }
        stop
    }

    /// Appends the wrap-up notice to the newest result once, when 80% of
    /// the run budget has elapsed.
    fn wrap_up(&mut self, outcomes: &mut [ToolOutcome]) {
        let due = !self.wrap_up_sent && self.clock.is_some_and(super::run::RunClock::wrapping_up);
        if let (true, Some(last)) = (due, outcomes.last_mut()) {
            last.result.content.push(Content::Text {
                text: WRAP_UP_NOTICE.to_owned(),
            });
            self.wrap_up_sent = true;
        }
    }

    /// Stores the results in call order and emits their events and the
    /// iteration's `turn.step`.
    async fn keep_results(
        &mut self,
        calls: &[RequestedCall],
        outcomes: &[ToolOutcome],
    ) -> Result<(), Error> {
        let mut steps = Vec::with_capacity(calls.len());
        for (call, outcome) in calls.iter().zip(outcomes) {
            self.append(&transcript::tool_row(call, &outcome.result))?;
            self.emit(Event::ToolResult(result_event(call, outcome)))
                .await;
            steps.push(StepCall {
                tool: call.name.clone(),
                args_digest: crate::sha256(call.arguments.as_bytes()),
                duration_ms: outcome.duration_ms,
                outcome: outcome.result.status.as_str().to_owned(),
            });
        }
        self.emit_step(steps).await;
        Ok(())
    }

    /// `turn.step` for the iteration just finished.
    pub(super) async fn emit_step(&self, calls: Vec<StepCall>) {
        self.emit(Event::TurnStep(TurnStep {
            iteration: self.iterations.used(),
            calls,
            usage: usage_payload(&self.usage),
        }))
        .await;
    }
}

fn result_event(call: &RequestedCall, outcome: &ToolOutcome) -> ToolResultEvent {
    let ok = matches!(outcome.result.status, Status::Ok | Status::Degraded);
    let text = outcome.result.text_content();
    ToolResultEvent {
        call_id: call.id.clone(),
        tool: call.name.clone(),
        ok,
        duration_ms: Some(outcome.duration_ms),
        output: ok.then(|| text.clone()),
        error: (!ok).then(|| pmt_obs::trajectory::payload::Failure {
            kind: "tool".to_owned(),
            code: Some(format!("tool.{}", outcome.result.status.as_str())),
            message: text,
        }),
    }
}

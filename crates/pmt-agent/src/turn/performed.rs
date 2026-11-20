//! Actions an agent runtime performed on its own (spec: model-providers,
//! Codex app-server runtime): the commands, patches and tool calls the
//! Codex app-server ran during a model call are recorded like a tool round
//! of the session's own (an assistant row with the calls, one tool row,
//! `tool.call` and `tool.result` per action and a `turn.step`), but never
//! run again. Each counts as one iteration, so the skill nudge sees the
//! work the runtime did.

use pmt_core::error::Error;
use pmt_obs::trajectory::events::Event;
use pmt_obs::trajectory::payload::{StepCall, ToolCall as ToolCallEvent};

use super::model::{ModelReply, RequestedCall};
use super::round::result_event;
use super::run::Turn;
use super::tools::ToolOutcome;
use super::transcript;

impl Turn<'_> {
    /// Records `reply`'s performed actions, before its answer.
    pub(super) async fn keep_performed(&mut self, reply: &ModelReply) -> Result<(), Error> {
        if reply.performed.is_empty() {
            return Ok(());
        }
        let calls: Vec<RequestedCall> = reply.performed.iter().map(|p| p.call.clone()).collect();
        let row = transcript::assistant_row(&ModelReply {
            tool_calls: calls,
            ..ModelReply::default()
        });
        self.append(&row)?;
        let nonce = pmt_core::provenance::nonce(&self.agent.setup.session.to_string());
        for performed in &reply.performed {
            let call = &performed.call;
            self.emit(Event::ToolCall(ToolCallEvent {
                call_id: call.id.clone(),
                tool: call.name.clone(),
                arguments: serde_json::from_str(&call.arguments)
                    .unwrap_or_else(|_| serde_json::Value::String(call.arguments.clone())),
            }))
            .await;
            let result = if performed.ok {
                pmt_tools::ToolResult::ok(performed.output.clone())
            } else {
                pmt_tools::ToolResult::error(performed.output.clone())
            };
            self.append(&transcript::tool_row(call, &result, &nonce))?;
            let outcome = ToolOutcome {
                result,
                duration_ms: 0,
            };
            self.emit(Event::ToolResult(result_event(call, &outcome)))
                .await;
            // Past the budget the turn still ends with this reply.
            let _ = self.iterations.consume();
            self.emit_step(vec![StepCall {
                tool: call.name.clone(),
                args_digest: crate::sha256(call.arguments.as_bytes()),
                duration_ms: 0,
                outcome: outcome.result.status.as_str().to_owned(),
            }])
            .await;
        }
        Ok(())
    }
}

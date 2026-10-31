//! The finalizer (spec: agent-turn-loop, Budget-exhaustion summary handoff;
//! Typed exit reasons): the summary handoff of a budget exit, `turn.end`
//! and the result.

use pmt_core::error::Error;
use pmt_obs::trajectory::events::Event;
use pmt_obs::trajectory::payload::TurnEnd;

use super::exit::TurnResult;
use super::run::{Ending, Turn, usage_payload};
use super::{HANDOFF_NOTICE, transcript};
use crate::prompt::frozen::Frozen;

/// Toolless handoff requests sent at most (one retry of an empty reply).
const HANDOFF_ATTEMPTS: usize = 2;

impl Turn<'_> {
    /// Runs the handoff when due, emits `turn.end` and builds the result.
    pub(super) async fn finish(
        &mut self,
        mut ending: Ending,
        frozen: Option<&Frozen>,
    ) -> Result<TurnResult, Error> {
        if let (true, Some(frozen)) = (ending.handoff, frozen) {
            ending.final_response = self.handoff(frozen).await?;
        }
        self.emit(Event::TurnEnd(TurnEnd {
            reason: ending.reason.as_str().to_owned(),
            iterations: self.iterations.used(),
            usage: usage_payload(&self.usage),
        }))
        .await;
        Ok(TurnResult {
            turn_id: self.id.clone(),
            final_response: ending.final_response,
            exit_reason: ending.reason,
            messages_appended: std::mem::take(&mut self.appended),
            usage: self.usage,
            iterations: self.iterations.used(),
            interrupt_issuer: ending.issuer,
            notices: std::mem::take(&mut self.notices),
            error: ending.error,
        })
    }

    /// Asks for a final summary without tools: exempt from the iteration
    /// limit and the budget check, but recorded like every call.
    async fn handoff(&mut self, frozen: &Frozen) -> Result<Option<String>, Error> {
        self.append(&transcript::notice_row(HANDOFF_NOTICE, "budget_handoff"))?;
        for _ in 0..HANDOFF_ATTEMPTS {
            let route = self.agent.setup.model.route().await;
            let request = self.request(frozen, &route, false)?;
            let Ok(reply) = self.call(&request).await else {
                return Ok(None); // interrupted or failed: keep the budget reason
            };
            self.record_call(&reply).await?;
            if !reply.text.trim().is_empty() {
                let mut kept = reply;
                kept.tool_calls.clear(); // a toolless request has no calls to answer
                self.keep_reply(&kept).await?;
                return Ok(Some(kept.text));
            }
        }
        Ok(None)
    }
}

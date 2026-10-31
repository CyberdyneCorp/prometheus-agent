//! The finalizer (spec: agent-turn-loop, Budget-exhaustion summary handoff;
//! Failed-turn boundary row; Typed exit reasons; Cache-hit reporting): the
//! summary handoff of a budget exit, the boundary row of a failed turn,
//! `turn.end` and the result.

use pmt_core::error::Error;
use pmt_obs::trajectory::events::Event;
use pmt_obs::trajectory::payload::TurnEnd;
use pmt_store::{NewMessage, Role, UsageScope};
use serde_json::json;

use super::exit::{ExitReason, TurnResult};
use super::run::{Ending, Turn, usage_payload};
use super::transcript::AUTHORED;
use super::{HANDOFF_NOTICE, transcript};
use crate::prompt::frozen::Frozen;

/// Toolless handoff requests sent at most (one retry of an empty reply).
const HANDOFF_ATTEMPTS: usize = 2;
/// The longest error text a boundary row carries.
const BOUNDARY_MAX_CHARS: usize = 2_000;

impl Turn<'_> {
    /// Runs the handoff when due, closes a failed turn, emits `turn.end`
    /// and builds the result. `frozen` is `None` for a turn that was never
    /// admitted (it holds no lease and must not write).
    pub(super) async fn finish(
        &mut self,
        mut ending: Ending,
        frozen: Option<&Frozen>,
    ) -> Result<TurnResult, Error> {
        if let (true, Some(frozen)) = (ending.handoff, frozen) {
            ending.final_response = self.handoff(frozen).await?;
        }
        if frozen.is_some() {
            self.close_failed(&ending)?;
        }
        if self.options.interrupt.is_hard() {
            self.agent.steers.discard();
        }
        let setup = &self.agent.setup;
        let cache_hit_rate = setup
            .store
            .usage_totals(UsageScope::Session(setup.session))
            .ok()
            .and_then(|totals| totals.tokens.cache_hit_rate());
        self.emit(Event::TurnEnd(TurnEnd {
            reason: ending.reason.as_str().to_owned(),
            iterations: self.iterations.used(),
            usage: usage_payload(&self.usage),
            cache_hit_rate,
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

    /// A turn that did not complete and left a user or tool row last gets
    /// an assistant boundary row with the redacted error, so the next user
    /// message never merges into the failed request (not for an overflow,
    /// which compaction handles). Once written, the last row is an
    /// assistant row, so a second call writes nothing.
    fn close_failed(&mut self, ending: &Ending) -> Result<(), Error> {
        let open = matches!(self.last_role, Some(Role::User | Role::Tool));
        if ending.reason == ExitReason::Completed
            || ending.reason == ExitReason::ContextExhausted
            || !open
        {
            return Ok(());
        }
        let detail = ending
            .error
            .as_ref()
            .map_or_else(|| ending.reason.as_str().to_owned(), ToString::to_string);
        let redacted = self.agent.setup.redactor.redact_all(&detail);
        let text: String = format!(
            "[prometheus] The turn ended ({}): {redacted}",
            ending.reason
        )
        .chars()
        .take(BOUNDARY_MAX_CHARS)
        .collect();
        let mut row = NewMessage::new(Role::Assistant, text);
        row.metadata = Some(
            json!({ AUTHORED: true, "kind": "failed_turn", "reason": ending.reason.as_str() }),
        );
        self.append(&row)
    }
}

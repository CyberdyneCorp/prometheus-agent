//! Steering a running turn (spec: agent-turn-loop, Steer placement
//! preserves the cache prefix).
//!
//! A steer is never added to a row that was already sent: pending steers
//! become one new user-role row after the newest tool result of the turn,
//! concatenated in arrival order and wrapped in out-of-band markers. A
//! steer still pending when the turn ends is prepended to the next turn's
//! user message, unless a hard interrupt discarded it. Only `note` steers
//! exist in M1; `redirect` and `wrap_up` arrive with structured steering.

use std::sync::{Mutex, PoisonError};

use pmt_core::error::{Error, codes};
use pmt_store::Role;

use super::run::Turn;
use super::transcript;

/// The kind every M1 steer has.
const KIND: &str = "note";

/// A session's undelivered steers, in arrival order.
#[derive(Debug, Default)]
pub(super) struct Steers {
    pending: Mutex<Vec<String>>,
}

impl Steers {
    /// Queues `text`.
    ///
    /// # Errors
    ///
    /// `turn.empty_steer` (`empty steer`) for empty or whitespace text.
    pub(super) fn push(&self, text: &str) -> Result<(), Error> {
        if text.trim().is_empty() {
            return Err(Error::new(&codes::TURN_EMPTY_STEER, "empty steer"));
        }
        self.lock().push(text.to_owned());
        Ok(())
    }

    /// Takes every pending steer as one wrapped block, if any.
    pub(super) fn take(&self) -> Option<String> {
        let pending = std::mem::take(&mut *self.lock());
        (!pending.is_empty()).then(|| wrap(&pending))
    }

    /// Drops every pending steer (a hard interrupt).
    pub(super) fn discard(&self) {
        self.lock().clear();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<String>> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// `texts` as one out-of-band block.
fn wrap(texts: &[String]) -> String {
    format!(
        "[OUT-OF-BAND USER MESSAGE kind={KIND}]\n{}\n[/OUT-OF-BAND USER MESSAGE]",
        texts.join("\n\n")
    )
}

impl Turn<'_> {
    /// Delivers pending steers as a new row when the newest stored row of
    /// this turn is a tool result; otherwise they wait.
    pub(super) fn deliver_steers(&mut self) -> Result<(), Error> {
        if self.last_role != Some(Role::Tool) {
            return Ok(());
        }
        if let Some(block) = self.agent.steers.take() {
            self.append(&transcript::notice_row(&block, "steer"))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steers_are_concatenated_in_arrival_order_and_empty_ones_rejected() {
        let steers = Steers::default();
        assert_eq!(steers.push("  ").unwrap_err().message(), "empty steer");
        steers.push("use the retry helper").unwrap();
        steers.push("and log it").unwrap();
        assert_eq!(
            steers.take().unwrap(),
            "[OUT-OF-BAND USER MESSAGE kind=note]\nuse the retry helper\n\nand log it\n[/OUT-OF-BAND USER MESSAGE]"
        );
        assert_eq!(steers.take(), None);
    }
}

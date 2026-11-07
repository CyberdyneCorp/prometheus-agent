//! Steering a running turn (spec: agent-turn-loop, Structured steering;
//! Steer placement preserves the cache prefix).
//!
//! A steer is never added to a row that was already sent: pending steers
//! become one new user-role row after the newest tool result of the turn,
//! concatenated in arrival order (consecutive steers of one kind share an
//! out-of-band wrapper). A steer still pending when the turn ends is
//! prepended to the next turn's user message, unless a hard interrupt
//! discarded it.
//!
//! The kinds: a `note` is a notice row; a `redirect` first asks the
//! running tool batch to yield (its cancel token fires, so cooperative
//! tools stop early) and is stored as a real user message; a `wrap_up`
//! lets the batch finish and then the loop makes one toolless request
//! whose answer ends the turn as `completed`.

use std::sync::{Mutex, MutexGuard, PoisonError};

use pmt_core::cancel::CancelToken;
use pmt_core::error::{Error, codes};
use pmt_store::{NewMessage, Role};
use serde_json::json;

use super::run::Turn;
use super::transcript;

/// How a steer is delivered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SteerKind {
    /// Inserted before the next model request.
    #[default]
    Note,
    /// Running cooperative tools yield; inserted as a real user message.
    Redirect,
    /// The batch finishes, then one toolless request ends the turn.
    WrapUp,
}

impl SteerKind {
    /// The wire name (`note`, `redirect`, `wrap_up`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Note => "note",
            Self::Redirect => "redirect",
            Self::WrapUp => "wrap_up",
        }
    }

    /// Parses a wire name.
    ///
    /// # Errors
    ///
    /// `rpc.invalid_params` naming the allowed values.
    pub fn parse(name: &str) -> Result<Self, Error> {
        match name {
            "note" => Ok(Self::Note),
            "redirect" => Ok(Self::Redirect),
            "wrap_up" => Ok(Self::WrapUp),
            other => Err(Error::new(
                &codes::RPC_INVALID_PARAMS,
                format!("unknown steer kind '{other}': expected note, redirect or wrap_up"),
            )),
        }
    }
}

#[derive(Debug)]
struct Pending {
    kind: SteerKind,
    text: String,
}

/// Pending steers taken for delivery.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Delivery {
    /// The wrapped block.
    pub(super) text: String,
    /// A `redirect` was among them: the row is a real user message.
    pub(super) redirect: bool,
    /// A `wrap_up` was among them.
    pub(super) wrap_up: bool,
}

/// A session's undelivered steers, in arrival order.
#[derive(Debug, Default)]
pub(super) struct Steers {
    pending: Mutex<Vec<Pending>>,
    /// The running tool batch's token (a redirect cancels it).
    batch: Mutex<Option<CancelToken>>,
}

impl Steers {
    /// Queues `text` as `kind`; a redirect asks the running batch to yield.
    ///
    /// # Errors
    ///
    /// `turn.empty_steer` (`empty steer`) for empty or whitespace text.
    pub(super) fn push(&self, text: &str, kind: SteerKind) -> Result<(), Error> {
        if text.trim().is_empty() {
            return Err(Error::new(&codes::TURN_EMPTY_STEER, "empty steer"));
        }
        lock(&self.pending).push(Pending {
            kind,
            text: text.to_owned(),
        });
        if kind == SteerKind::Redirect
            && let Some(batch) = lock(&self.batch).as_ref()
        {
            batch.cancel();
        }
        Ok(())
    }

    /// Takes every pending steer as one delivery, if any.
    pub(super) fn take(&self) -> Option<Delivery> {
        let pending = std::mem::take(&mut *lock(&self.pending));
        (!pending.is_empty()).then(|| Delivery {
            text: wrap(&pending),
            redirect: pending.iter().any(|p| p.kind == SteerKind::Redirect),
            wrap_up: pending.iter().any(|p| p.kind == SteerKind::WrapUp),
        })
    }

    /// Takes every pending steer's text as sent.
    pub(super) fn drain(&self) -> Vec<String> {
        std::mem::take(&mut *lock(&self.pending))
            .into_iter()
            .map(|p| p.text)
            .collect()
    }

    /// Drops every pending steer (a hard interrupt).
    pub(super) fn discard(&self) {
        lock(&self.pending).clear();
    }

    /// Registers (or clears) the running tool batch's token; a redirect
    /// that is already pending cancels it at once.
    pub(super) fn watch_batch(&self, token: Option<CancelToken>) {
        let redirecting = lock(&self.pending)
            .iter()
            .any(|p| p.kind == SteerKind::Redirect);
        if let (true, Some(token)) = (redirecting, &token) {
            token.cancel();
        }
        *lock(&self.batch) = token;
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The steers as out-of-band blocks, consecutive steers of one kind
/// sharing a wrapper.
fn wrap(pending: &[Pending]) -> String {
    pending
        .chunk_by(|a, b| a.kind == b.kind)
        .map(|run| {
            let texts: Vec<&str> = run.iter().map(|p| p.text.as_str()).collect();
            format!(
                "[OUT-OF-BAND USER MESSAGE kind={}]\n{}\n[/OUT-OF-BAND USER MESSAGE]",
                run[0].kind.as_str(),
                texts.join("\n\n")
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The row a delivery becomes: a notice row, or a real user message for a
/// redirect.
fn row(delivery: &Delivery) -> NewMessage {
    if delivery.redirect {
        let mut row = NewMessage::new(Role::User, delivery.text.clone());
        row.metadata = Some(json!({ "kind": "steer", "steer": "redirect" }));
        row
    } else {
        transcript::notice_row(&delivery.text, "steer")
    }
}

impl Turn<'_> {
    /// Delivers pending steers as a new row when the newest stored row of
    /// this turn is a tool result; otherwise they wait. `true` when a
    /// `wrap_up` was delivered.
    pub(super) fn deliver_steers(&mut self) -> Result<bool, Error> {
        if self.last_role != Some(Role::Tool) {
            return Ok(false);
        }
        let Some(delivery) = self.agent.steers.take() else {
            return Ok(false);
        };
        self.append(&row(&delivery))?;
        Ok(delivery.wrap_up)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steers_are_concatenated_in_arrival_order_and_empty_ones_rejected() {
        let steers = Steers::default();
        let err = steers.push("  ", SteerKind::Note).unwrap_err();
        assert_eq!(err.message(), "empty steer");
        steers
            .push("use the retry helper", SteerKind::Note)
            .unwrap();
        steers.push("and log it", SteerKind::Note).unwrap();
        let delivery = steers.take().unwrap();
        assert_eq!(
            delivery.text,
            "[OUT-OF-BAND USER MESSAGE kind=note]\nuse the retry helper\n\nand log it\n[/OUT-OF-BAND USER MESSAGE]"
        );
        assert!(!delivery.redirect && !delivery.wrap_up);
        assert_eq!(steers.take(), None);
    }

    #[test]
    fn mixed_kinds_keep_arrival_order_under_their_own_wrappers() {
        let steers = Steers::default();
        steers.push("a", SteerKind::Note).unwrap();
        steers.push("b", SteerKind::Redirect).unwrap();
        steers.push("c", SteerKind::Note).unwrap();
        let delivery = steers.take().unwrap();
        assert_eq!(
            delivery.text,
            "[OUT-OF-BAND USER MESSAGE kind=note]\na\n[/OUT-OF-BAND USER MESSAGE]\n\n\
             [OUT-OF-BAND USER MESSAGE kind=redirect]\nb\n[/OUT-OF-BAND USER MESSAGE]\n\n\
             [OUT-OF-BAND USER MESSAGE kind=note]\nc\n[/OUT-OF-BAND USER MESSAGE]"
        );
        assert!(delivery.redirect);
    }

    #[test]
    fn a_redirect_cancels_the_running_batch() {
        let steers = Steers::default();
        let batch = CancelToken::new();
        steers.watch_batch(Some(batch.clone()));
        steers.push("note only", SteerKind::Note).unwrap();
        assert!(!batch.is_cancelled());
        steers.push("stop and do X", SteerKind::Redirect).unwrap();
        assert!(batch.is_cancelled());
    }

    #[test]
    fn kinds_parse_by_wire_name() {
        assert_eq!(SteerKind::parse("wrap_up").unwrap(), SteerKind::WrapUp);
        assert!(SteerKind::parse("later").is_err());
    }
}

//! Per-turn memory recall on the wire only (spec: memory, Per-turn recall
//! injection; agent-turn-loop, Request-time additions never touch history).
//!
//! The recall block rides on this turn's user message in every request of
//! the turn and is never stored: the stored row keeps only the user's
//! text. The next turn therefore sends that message without the block, an
//! expected change the cache-break check must not report, so the canonical
//! prefix digests the message as stored.

use pmt_obs::trajectory::events::Event;
use pmt_obs::trajectory::payload::MemoryRecalled;
use pmt_providers::request::{Message, Part, Role};

use super::run::Turn;

/// This turn's recall block and the user message it rides on.
#[derive(Debug, Clone)]
pub(super) struct Ephemeral {
    /// The user row's stored wire text.
    wire: String,
    /// The block appended to it.
    block: String,
    /// The message index in the latest request.
    pub(super) index: Option<usize>,
}

impl Ephemeral {
    /// The message as stored (what the next turn will send).
    pub(super) fn stored(&self) -> Message {
        Message::text(Role::User, self.wire.clone())
    }

    /// Appends the block to the newest user message that is this turn's
    /// row and remembers its index.
    pub(super) fn attach(&mut self, messages: &mut [Message]) {
        self.index = messages.iter().rposition(|m| {
            m.role == Role::User
                && matches!(m.parts.as_slice(), [Part::Text { text }] if *text == self.wire)
        });
        if let Some(Part::Text { text }) = self.index.and_then(|i| messages[i].parts.first_mut()) {
            text.push_str("\n\n");
            text.push_str(&self.block);
        }
    }
}

impl Turn<'_> {
    /// Asks the session's memory for this turn's recall of `text` (the
    /// stored user row is `wire`).
    pub(super) async fn recall(&mut self, text: &str, wire: &str) {
        let Some(memory) = self.agent.setup.memory.clone() else {
            return;
        };
        let Some(recalled) = memory.begin_turn(text) else {
            return;
        };
        self.ephemeral = Some(Ephemeral {
            wire: wire.to_owned(),
            block: recalled.text,
            index: None,
        });
        self.emit(Event::MemoryRecalled(MemoryRecalled {
            entry_ids: recalled.entry_ids,
        }))
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_block_rides_on_the_matching_user_message_only() {
        let mut e = Ephemeral {
            wire: "which DB?".into(),
            block: "<memory-context>x</memory-context>".into(),
            index: None,
        };
        let mut messages = vec![
            Message::text(Role::User, "which DB?"),
            Message::text(Role::Assistant, "Postgres."),
            Message::text(Role::User, "which DB?"),
        ];
        e.attach(&mut messages);
        assert_eq!(e.index, Some(2));
        assert_eq!(messages[0], Message::text(Role::User, "which DB?"));
        assert_eq!(
            messages[2],
            Message::text(
                Role::User,
                "which DB?\n\n<memory-context>x</memory-context>"
            )
        );
        assert_eq!(e.stored(), Message::text(Role::User, "which DB?"));
    }
}

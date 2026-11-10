//! What the TUI shows as sessions open, close and as it leaves (spec:
//! memory, Reviewable memory changes per session; session-storage,
//! Low-disk prune prompt): the memory review summary of a session that is
//! resumed, closed or left, and at startup the low-disk prune question on
//! a card that `on`, `off` or `later` (Enter alone) answers through
//! `/autoprune`. The daemon owns the store, so both are asked of it.

use pmt_tui::app::Card;

use super::link::Link;
use super::screen::Tui;
use crate::chat::autoprune::Answer;
use crate::daemon_client;

impl Tui<'_> {
    /// Shows `session`'s memory summary, when it has one.
    pub(super) async fn memory_summary(&mut self, session: &str) {
        if let Some(line) = daemon_client::memory_summary(&self.link.client, session).await {
            self.notice(line);
        }
    }

    /// At startup: the low-disk prune card, when it is due.
    pub(super) async fn offer_prune(&mut self) {
        if let Some(question) = daemon_client::autoprune_offer(&self.link.client).await {
            self.notice(question);
            self.prune = true;
            self.show_question();
        }
    }

    /// The card asking about auto-prune, while it waits.
    pub(super) fn prune_card(&self) -> Option<Card> {
        self.prune.then(|| Card {
            title: "turn on auto-prune of sessions inactive for 90 days?".to_owned(),
            body: String::new(),
            hint: "type on, off (never ask again) or later; Enter alone is later".to_owned(),
        })
    }

    /// The answer to the prune card; `false` when `line` is no answer (it
    /// is then sent as usual and the card stays).
    pub(super) fn answer_prune(&mut self, line: &str) -> bool {
        let Some(answer) = Answer::parse(line) else {
            return false;
        };
        self.prune = false;
        let word = match answer {
            Answer::On => "on",
            Answer::Off => "off",
            Answer::Later => "later",
        };
        self.dispatch("autoprune".to_owned(), word.to_owned());
        self.show_question();
        true
    }
}

/// The memory summaries of the sessions a leaving TUI held, to print once
/// the terminal is restored; each names its session when it held several.
pub(super) async fn summaries(link: &Link) -> Vec<String> {
    let mut lines = Vec::new();
    for session in &link.live {
        if let Some(line) = daemon_client::memory_summary(&link.client, session).await {
            lines.push(if link.live.len() > 1 {
                format!("session {session}: {line}")
            } else {
                line
            });
        }
    }
    lines
}

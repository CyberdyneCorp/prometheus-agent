//! `manage_connections` run by the turn loop (spec: connectors, Connection
//! flows require the user): the tool's logic is
//! `pmt_tools::connectors::manage`; this is its connection card, asked over
//! the same human-only channel as `clarify`.

use std::time::Duration;

use pmt_tools::connectors::manage::{Card, CardAnswer, CardTarget};
use pmt_tools::search::RemoteFuture;

use super::clarify::{AnswerStatus, Clarifier, Question};

/// The choice that says a target is done.
const DONE: &str = "Done";
/// The choice that skips it.
const SKIP: &str = "Skip";

/// The connection card shown through a session's clarify channel: one
/// question per target with its link, answered `Done` or `Skip`.
pub struct ClarifyCard<'a> {
    clarifier: &'a Clarifier,
}

impl<'a> ClarifyCard<'a> {
    /// The card of `clarifier`'s surface.
    #[must_use]
    pub fn new(clarifier: &'a Clarifier) -> Self {
        Self { clarifier }
    }
}

impl Card for ClarifyCard<'_> {
    fn show<'a>(&'a self, targets: &'a [CardTarget]) -> RemoteFuture<'a, Vec<CardAnswer>> {
        Box::pin(async move {
            let questions = targets
                .iter()
                .map(|t| {
                    let account = t
                        .alias
                        .as_ref()
                        .map(|a| format!(" (account '{a}')"))
                        .unwrap_or_default();
                    Question {
                        question: format!(
                            "Connect {}{account}: open {} and sign in, then choose {DONE}.",
                            t.connector, t.url
                        ),
                        choices: vec![DONE.to_owned(), SKIP.to_owned()],
                        multi: false,
                    }
                })
                .collect();
            // The tool bounds the wait itself (connectors.connect_timeout_secs).
            match self.clarifier.ask(questions, Duration::MAX).await {
                Ok(reply) => targets
                    .iter()
                    .enumerate()
                    .map(|(i, _)| match reply.answers.get(i) {
                        Some(a) if a.status == AnswerStatus::Skipped => CardAnswer::Skipped,
                        Some(a) if a.choices.iter().any(|c| c == SKIP) => CardAnswer::Skipped,
                        Some(_) => CardAnswer::Done,
                        None => CardAnswer::Unanswered,
                    })
                    .collect(),
                Err(_) => vec![CardAnswer::Unanswered; targets.len()],
            }
        })
    }
}

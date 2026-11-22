//! `catalog_manage` run by the turn loop (spec: plugin-system, Catalog
//! tool for agent-initiated installs): the tool's logic is
//! `pmt_tools::catalog_manage`; this is its approval card, one row per
//! item, asked over the session's human-only channel (the same as
//! `clarify`, which only desktop and TUI sessions of this tool draw).

use std::time::Duration;

use pmt_tools::catalog_manage::{InstallCard, Item};
use pmt_tools::search::RemoteFuture;

use super::clarify::{AnswerStatus, Clarifier, Question};

/// The choice that installs a row.
const INSTALL: &str = "Install";
/// The choice that skips it.
const SKIP: &str = "Skip";

/// The install card shown through a session's clarify channel.
pub struct ClarifyInstallCard<'a> {
    clarifier: &'a Clarifier,
}

impl<'a> ClarifyInstallCard<'a> {
    /// The card of `clarifier`'s surface.
    #[must_use]
    pub fn new(clarifier: &'a Clarifier) -> Self {
        Self { clarifier }
    }
}

impl InstallCard for ClarifyInstallCard<'_> {
    fn show<'a>(&'a self, items: &'a [Item], reason: &'a str) -> RemoteFuture<'a, Vec<bool>> {
        Box::pin(async move {
            let why = if reason.is_empty() {
                String::new()
            } else {
                format!(" ({reason})")
            };
            let questions = items
                .iter()
                .map(|item| Question {
                    question: format!("Install {} '{}'?{why}", item.kind.as_str(), item.id),
                    choices: vec![INSTALL.to_owned(), SKIP.to_owned()],
                    multi: false,
                })
                .collect();
            // The card waits for the user; nothing installs without an answer.
            match self.clarifier.ask(questions, Duration::MAX).await {
                Ok(reply) => (0..items.len())
                    .map(|i| {
                        reply.answers.get(i).is_some_and(|a| {
                            a.status != AnswerStatus::Skipped
                                && a.choices.iter().any(|c| c == INSTALL)
                        })
                    })
                    .collect(),
                Err(_) => vec![false; items.len()],
            }
        })
    }
}

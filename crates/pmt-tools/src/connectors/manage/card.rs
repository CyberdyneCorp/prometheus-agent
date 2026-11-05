//! The connection card: on surfaces with one (a CLI terminal, the TUI, the
//! desktop) the user sees each authorization link and finishes or skips
//! it; the tool waits for that, for the gateway to report every account
//! connected, or for `connectors.connect_timeout_secs`, and the model only
//! ever sees the outcome (never a link).

use std::time::Duration;

use serde_json::{Value, json};

use super::Pending;
use crate::connectors::client::Gateway;
use crate::connectors::wire::Account;
use crate::search::RemoteFuture;

/// One target as the card shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardTarget {
    /// Connector slug.
    pub connector: String,
    /// The account's alias, when named.
    pub alias: Option<String>,
    /// The link the user opens.
    pub url: String,
}

/// What the user did with one target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardAnswer {
    /// Says it is done.
    Done,
    /// Skipped it.
    Skipped,
    /// Gave no answer (dismissed, timed out, no one there).
    Unanswered,
}

/// A surface's connection card.
pub trait Card: Send + Sync {
    /// Shows `targets` and answers one entry per target, in order.
    fn show<'a>(&'a self, targets: &'a [CardTarget]) -> RemoteFuture<'a, Vec<CardAnswer>>;
}

/// Shows the card for `pending` and waits; one outcome per target.
pub(super) async fn wait(
    card: &dyn Card,
    gateway: &Gateway,
    pending: &[Pending],
    timeout: Duration,
    poll: Duration,
) -> Vec<Value> {
    let targets: Vec<CardTarget> = pending
        .iter()
        .map(|p| CardTarget {
            connector: p.connector.clone(),
            alias: p.alias.clone(),
            url: p.url.clone(),
        })
        .collect();
    let answered = tokio::time::timeout(timeout, async {
        tokio::select! {
            answers = card.show(&targets) => Some(answers),
            () = all_connected(gateway, pending, poll) => None,
        }
    })
    .await
    .ok()
    .flatten();
    let accounts = gateway.accounts().await.unwrap_or_default();
    pending
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let status = if p.connected_in(&accounts) {
                "connected"
            } else if answered
                .as_ref()
                .and_then(|a| a.get(i))
                .is_some_and(|a| *a == CardAnswer::Skipped)
            {
                "skipped"
            } else {
                "not_connected"
            };
            p.outcome(status)
        })
        .collect()
}

/// Returns once every pending account is connected.
async fn all_connected(gateway: &Gateway, pending: &[Pending], poll: Duration) {
    loop {
        tokio::time::sleep(poll).await;
        if let Ok(accounts) = gateway.accounts().await
            && pending.iter().all(|p| p.connected_in(&accounts))
        {
            return;
        }
    }
}

impl Pending {
    /// Whether `accounts` show this authorization finished.
    pub(super) fn connected_in(&self, accounts: &[Account]) -> bool {
        accounts.iter().any(|a| {
            a.is_active()
                && match &self.connection_id {
                    Some(id) => &a.connection_id == id,
                    None => a.connector == self.connector && a.alias == self.alias,
                }
        })
    }

    pub(super) fn outcome(&self, status: &str) -> Value {
        let mut outcome = json!({"connector": self.connector, "status": status});
        if let Some(alias) = &self.alias {
            outcome["alias"] = json!(alias);
        }
        outcome
    }
}

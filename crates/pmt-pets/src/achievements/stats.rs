//! One session's statistics, read from its stored messages and usage
//! rows; cached per `(session_id, started_at, last_active)` so a session
//! is analysed again only when it changed.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use pmt_core::error::Error;
use pmt_store::{Role, Store, Visibility};
use serde::{Deserialize, Serialize};

/// What one session contributes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionStats {
    /// Start time, Unix milliseconds.
    pub started_at_ms: i64,
    /// User and assistant messages.
    pub messages: u64,
    /// Calls per tool name.
    pub tools: BTreeMap<String, u64>,
    /// Most tool calls within one turn.
    pub longest_chain: u64,
    /// Failed tool calls.
    pub errors: u64,
    /// Failed calls a later call of the same tool got right.
    pub recoveries: u64,
    /// Models of main-model calls.
    pub models: BTreeSet<String>,
    /// Providers of main-model calls.
    pub providers: BTreeSet<String>,
}

impl SessionStats {
    /// Calls of the tools `matches` accepts.
    #[must_use]
    pub fn calls(&self, matches: impl Fn(&str) -> bool) -> u64 {
        self.tools
            .iter()
            .filter(|(name, _)| matches(name))
            .map(|(_, n)| n)
            .sum()
    }
}

/// Analyses `session` of `store`.
///
/// # Errors
///
/// The store's read errors.
pub fn analyse(store: &Store, session: &pmt_store::Session) -> Result<SessionStats, Error> {
    let mut stats = SessionStats {
        started_at_ms: session.started_at.as_millisecond(),
        ..SessionStats::default()
    };
    let mut chain = 0_u64;
    let mut failed: HashMap<String, u64> = HashMap::new();
    for message in store.messages(session.id, Visibility::All)? {
        match message.role {
            Role::User => {
                stats.messages += 1;
                chain = 0;
            }
            Role::Assistant => {
                stats.messages += 1;
                for name in called(message.tool_calls.as_ref()) {
                    *stats.tools.entry(name).or_default() += 1;
                    chain += 1;
                    stats.longest_chain = stats.longest_chain.max(chain);
                }
            }
            Role::Tool => {
                let name = message.tool_name.clone().unwrap_or_default();
                let is_error = message
                    .metadata
                    .as_ref()
                    .is_some_and(|m| m["is_error"] == serde_json::Value::Bool(true));
                if is_error {
                    stats.errors += 1;
                    *failed.entry(name).or_default() += 1;
                } else if let Some(open) = failed.get_mut(&name).filter(|n| **n > 0) {
                    stats.recoveries += *open;
                    *open = 0;
                }
            }
            Role::System => {}
        }
    }
    for row in store.usage_events(session.id)? {
        if row.purpose == "main" {
            stats.models.insert(row.model);
            stats.providers.insert(row.provider);
        }
    }
    Ok(stats)
}

fn called(calls: Option<&serde_json::Value>) -> Vec<String> {
    calls
        .and_then(serde_json::Value::as_array)
        .map(|calls| {
            calls
                .iter()
                .filter_map(|c| c["name"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

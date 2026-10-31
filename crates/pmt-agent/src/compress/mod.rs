//! Context compression (spec: context-compression).
//!
//! A long session stays inside the main model's window through in-place
//! compaction: [`threshold`] says when, [`decision`] whether the guards
//! (cooldown, backoff, the `ineffective` breaker) allow it, [`split`]
//! picks the protected head and the token-budgeted tail, [`prune`]
//! shrinks old tool output deterministically (originals kept in the blob
//! store), [`summary`] asks the auxiliary `compression` route for the
//! handoff summary of the middle (or builds the deterministic fallback),
//! [`assemble`] places it with strict role alternation and stubs orphaned
//! calls, and [`compactor`] commits the result under the session's
//! compaction lock. [`engine`] selects the context engine and [`manual`]
//! parses `/compress`.
//!
//! Compaction is the only sanctioned rewrite of already-sent context: the
//! turn loop rebuilds the system prompt after every commit and the cache
//! break detector treats the commit as expected.

pub mod assemble;
pub mod compactor;
pub mod decision;
pub mod engine;
#[cfg(test)]
pub(crate) mod fixtures;
pub mod items;
pub mod manual;
pub mod prune;
pub mod split;
pub mod summary;
pub mod threshold;

use pmt_store::{Message as Row, Role};
use serde_json::Value;

use crate::turn::transcript::AUTHORED;

pub use compactor::{Compactor, Outcome, Run, Status};
pub use summary::{FailureKind, Summarizer, SummaryFailure};

/// Metadata flag of a summary row (never sent on the wire).
pub const SUMMARY_FLAG: &str = "compaction_summary";

/// What started a compaction attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// `/compress`.
    Manual,
    /// The check at the start of a turn.
    TurnStart,
    /// The check before a model request.
    PreRequest,
    /// The check after a tool round.
    PostTool,
    /// The provider rejected the request as too long.
    Overflow,
    /// The gateway's pre-turn hygiene (M2).
    GatewayHygiene,
}

impl Trigger {
    /// The `compression.committed` label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::TurnStart => "turn_start",
            Self::PreRequest => "pre_request",
            Self::PostTool => "post_tool",
            Self::Overflow => "overflow",
            Self::GatewayHygiene => "gateway_hygiene",
        }
    }
}

/// A rough token estimate of `text` (4 bytes per token, as the request
/// estimate counts).
#[must_use]
pub fn tokens(text: &str) -> u64 {
    (text.len() as u64).div_ceil(4)
}

/// The estimated tokens of one stored row: its text and its tool calls.
#[must_use]
pub fn row_tokens(row: &Row) -> u64 {
    let calls = row.tool_calls.as_ref().map_or(0, |c| c.to_string().len());
    let text = row.content.as_deref().map_or(0, str::len);
    ((text + calls) as u64).div_ceil(4)
}

/// The estimated tokens of `rows`.
#[must_use]
pub fn rows_tokens(rows: &[Row]) -> u64 {
    rows.iter().map(row_tokens).sum()
}

fn flag(row: &Row, key: &str) -> bool {
    row.metadata
        .as_ref()
        .and_then(|m| m.get(key))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Whether `row` is a compaction summary.
#[must_use]
pub fn is_summary(row: &Row) -> bool {
    flag(row, SUMMARY_FLAG)
}

/// Whether `row` is a message the user wrote (not a notice Prometheus
/// added, not a summary).
#[must_use]
pub fn is_real_user(row: &Row) -> bool {
    row.role == Role::User && !flag(row, AUTHORED) && !is_summary(row)
}

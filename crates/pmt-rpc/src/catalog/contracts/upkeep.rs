//! What an interactive client shows as a session opens or ends, read from
//! the daemon because it owns the store: the session's memory review
//! summary (spec: memory, Reviewable memory changes per session) and the
//! low-disk prune question (spec: session-storage, Low-disk prune prompt).
//! The daemon cannot push either: a resumed session's events start after
//! the client attaches, and an ending one has no client left to tell.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{Method, SessionRef};

/// `memory.summary` result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MemorySummary {
    /// `Memory: +<added> ~<patched> -<removed> (review with /memory
    /// review)` when the agent changed memory in the session and
    /// `memory.review_mode` is not `off`; `null` otherwise.
    #[serde(default)]
    pub line: Option<String>,
}

/// `autoprune.offer` params.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AutopruneOfferParams {
    /// The profile (the connection's only one when omitted).
    #[serde(default)]
    pub profile: Option<String>,
}

/// `autoprune.offer` result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AutopruneOffer {
    /// The question to ask (`on`, `off` or `later`, answered with
    /// `/autoprune <answer>`), when it is due; `null` otherwise. A
    /// non-null answer counts as today's check.
    #[serde(default)]
    pub question: Option<String>,
}

method!(/// `memory.summary`: the one-line summary of the agent's memory changes
    /// in a session, shown when it ends or is resumed.
    MemorySummaryCall = "memory.summary" (SessionRef) -> MemorySummary);
method!(/// `autoprune.offer`: the low-disk prune question, at most once a day,
    /// for an interactive client's startup.
    AutopruneOfferCall = "autoprune.offer" (AutopruneOfferParams) -> AutopruneOffer);

/// `daemon.update` params.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DaemonUpdateParams {
    /// Only look for an update (`prometheus update --check`).
    #[serde(default)]
    pub check: bool,
}

/// `daemon.update` result. A refusal (another owner such as a package
/// manager or the container image, the update lock held) is an error
/// with the updater's own code and message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DaemonUpdated {
    /// `started` (the updater runs detached and hands the daemon over
    /// when it finishes) or `checked`.
    pub status: String,
    /// What the check printed, or where the updater writes its log.
    pub message: String,
}

method!(/// `daemon.update`: updates this daemon's install through the same
    /// code path as `prometheus update` (spec: desktop-app, Update every
    /// connected instance); only for principals the daemon grants admin.
    DaemonUpdate = "daemon.update" (DaemonUpdateParams) -> DaemonUpdated);

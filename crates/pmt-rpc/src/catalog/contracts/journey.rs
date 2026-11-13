//! The learning journey (spec: memory, Learning journey timeline): the
//! skills and memory entries the agent learned, oldest first, for the TUI
//! overlay and the desktop star map. `prometheus journey --json` prints
//! the same nodes.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::Method;

/// `journey.list` params.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JourneyListParams {
    /// The profile (the connection's only one when omitted).
    #[serde(default)]
    pub profile: Option<String>,
    /// Only nodes created at or after this: a duration back from now
    /// (`2h`, `3d`, `1w`), a date (`2026-11-01`) or a timestamp.
    #[serde(default)]
    pub since: Option<String>,
}

/// One learned skill or memory entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JourneyNode {
    /// `skill:<name>` for a skill, the entry id (`m_...`) for memory.
    pub id: String,
    /// `skill` or `memory`.
    pub kind: String,
    /// The skill's name or the entry's text (one line, shortened).
    pub title: String,
    /// When it was learned (RFC 3339).
    pub created_at: String,
    /// When it last changed (RFC 3339).
    pub updated_at: String,
    /// Who created it: a skill's `user`, `agent`, `hub` or `bundled` (a
    /// used bundled skill); an entry's `user`, `agent`,
    /// `background_review`, `provider`, `consolidation` or `import`.
    pub origin: String,
    /// How often it was used: a skill's loads, an entry's recalls (the
    /// memory graph's `used` filter).
    #[serde(default)]
    pub uses: u64,
}

/// `journey.list` result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JourneyList {
    /// The nodes, oldest first.
    pub nodes: Vec<JourneyNode>,
}

method!(/// `journey.list`: the learning journey's nodes, oldest first.
    JourneyListCall = "journey.list" (JourneyListParams) -> JourneyList);

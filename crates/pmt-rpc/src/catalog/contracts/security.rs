//! Contracts of the security boundaries (spec: security-boundaries,
//! Per-session network egress policy).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::Method;

/// `egress.set` params: changes applied in order (mode, then allow, then
/// deny); an empty change only reads the policy.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EgressSetParams {
    /// The session.
    pub session_id: String,
    /// `public`, `allowlist` or `none`.
    #[serde(default)]
    pub mode: Option<String>,
    /// Host patterns to admit (`host`, `host:port`, `*.suffix`).
    #[serde(default)]
    pub allow: Vec<String>,
    /// Host patterns to refuse in every mode.
    #[serde(default)]
    pub deny: Vec<String>,
}

/// `egress.set` result: the session's policy after the change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EgressState {
    /// The mode.
    pub mode: String,
    /// Admitted patterns.
    pub allow: Vec<String>,
    /// Refused patterns.
    pub deny: Vec<String>,
}

method!(/// `egress.set`: narrows or extends a session's egress policy; the
    /// system prompt and tool array stay as they are.
    EgressSet = "egress.set" (EgressSetParams) -> EgressState);

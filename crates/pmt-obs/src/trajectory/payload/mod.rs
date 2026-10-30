//! Typed payloads of the trajectory event types (spec: observability,
//! Trajectory event types). `schema/trajectory/v1.json` is generated from
//! these structs (`cargo xtask gen`).
//!
//! Minor grammar versions only add event types or optional fields, so no
//! payload denies unknown fields and every field added after 1.0 must be an
//! `Option` (or carry a serde default).

mod agent;
mod evolution;

pub use agent::*;
pub use evolution::*;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Token counts by class, as in a usage-ledger row.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Tokens {
    /// Uncached input tokens.
    pub input: u64,
    /// Output tokens.
    pub output: u64,
    /// Input tokens read from the provider's prompt cache.
    pub cache_read: u64,
    /// Input tokens written to the provider's prompt cache.
    pub cache_write: u64,
    /// Reasoning tokens (part of output where the provider reports them).
    pub reasoning: u64,
}

/// Cumulative usage carried by step and progress events.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Usage {
    /// Tokens by class.
    pub tokens: Tokens,
    /// Cost in USD, `null` when a call was unpriced.
    pub cost_usd: Option<f64>,
}

/// A typed failure: the error taxonomy's kind and code plus a message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Failure {
    /// Error kind (`not_found`, `permission_denied`, `tool`...).
    pub kind: String,
    /// Error code, when the failure has one (`fs.not_found`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// Human-readable message.
    pub message: String,
}

/// `error`: a failure that is not the result of one tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ErrorEvent {
    /// Error code (for example `trajectory.size_limit`).
    pub code: String,
    /// Error kind.
    pub kind: String,
    /// Human-readable message.
    pub message: String,
}

/// `budget.warning` and `budget.exceeded`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Budget {
    /// `turn`, `session`, `root`, `daily`, `monthly` or `unattended`.
    pub scope: String,
    /// `usd` or `tokens`.
    pub unit: String,
    /// The configured limit.
    pub limit: f64,
    /// Spent so far in the scope's period.
    pub spent: f64,
    /// Worst-case estimate of the refused call (`budget.exceeded` only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimate: Option<f64>,
}

/// `egress.denied`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EgressDenied {
    /// Destination host.
    pub host: String,
    /// Destination port.
    pub port: u16,
    /// Who asked (tool name, plugin id, `browser`).
    pub requester: String,
    /// Why (`private_range`, `not_allowlisted`, `mode_none`...).
    pub reason: String,
}

/// `security.downgrade`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SecurityDowngrade {
    /// The degraded feature (`landlock`, `seccomp`...).
    pub feature: String,
    /// Why it is unavailable.
    pub reason: String,
}

#[cfg(test)]
mod tests {
    use super::{Failure, ToolResult};

    #[test]
    fn a_failed_tool_result_carries_the_error_kind() {
        let result = ToolResult {
            call_id: "call_1".to_owned(),
            tool: "read_file".to_owned(),
            ok: false,
            duration_ms: Some(3),
            output: None,
            error: Some(Failure {
                kind: "not_found".to_owned(),
                code: None,
                message: "no such file: notes.txt".to_owned(),
            }),
        };
        let value = serde_json::to_value(&result).unwrap();
        assert_eq!(value["ok"], false);
        assert_eq!(value["error"]["kind"], "not_found");
        assert!(value.get("output").is_none());
    }
}

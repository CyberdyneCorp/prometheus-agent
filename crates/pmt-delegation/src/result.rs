//! The structured result of one child (spec: subagent-delegation,
//! Structured result contract): contract version 1, published as a JSON
//! Schema by `prometheus schema subagent-result`.

use pmt_obs::trajectory::payload::Tokens;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The contract version this build writes.
pub const CONTRACT_VERSION: u32 = 1;

/// How a child ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// It gave an answer (possibly truncated by a budget).
    Completed,
    /// It failed.
    Failed,
    /// It was stopped.
    Interrupted,
    /// It made no progress for too long.
    Stalled,
    /// It never ran (a router or route refused its task).
    Refused,
}

impl Status {
    /// The wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
            Self::Stalled => "stalled",
            Self::Refused => "refused",
        }
    }
}

/// Why its loop stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExitReason {
    /// The model gave a final answer.
    FinalAnswer,
    /// The iteration budget ran out.
    MaxIterations,
    /// A token or cost budget ran out.
    BudgetExhausted,
    /// It was stopped.
    Interrupted,
    /// Something failed.
    Error,
}

/// A file the child wrote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Artifact {
    /// The path as written.
    pub path: String,
    /// SHA-256 of its content now (lower-case hex).
    pub sha256: String,
    /// `blobs/<sha256>` for files of at most 1 MiB (kept in the store).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>,
}

/// What the child used.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ResultUsage {
    /// Tokens by kind.
    pub tokens: Tokens,
    /// Model calls.
    pub iterations: u32,
    /// Tool calls.
    pub tool_calls: u32,
}

/// A failure's code and message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ResultError {
    /// Stable error code.
    pub code: String,
    /// What happened.
    pub message: String,
}

/// A child's isolation worktree on finish (spec: git-worktrees, Subagent
/// worktree isolation).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorktreeReport {
    /// Where it ran (`<repo>/.worktrees/subagent-<id>`).
    pub path: String,
    /// Its branch (`<prefix>/subagent-<id>`), kept for the parent to merge
    /// when it holds work.
    pub branch: String,
    /// The commits it made (`<short sha> <subject>`), oldest first.
    pub commits: Vec<String>,
    /// It left uncommitted edits.
    pub dirty: bool,
    /// The tree and its branch were removed (nothing to keep).
    pub pruned: bool,
    /// The archive holding its work, when it was archived.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive_ref: Option<String>,
    /// The tree could not be inspected, so it was kept in place.
    pub inspection_failed: bool,
}

/// One child's result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[schemars(title = "SubagentResult")]
pub struct SubagentResult {
    /// Always 1.
    pub contract_version: u32,
    /// `sa-<ULID>`.
    pub subagent_id: String,
    /// Its task's index in the call.
    pub task_index: usize,
    /// How it ended.
    pub status: Status,
    /// Why its loop stopped.
    pub exit_reason: ExitReason,
    /// A budget cut the work short.
    pub truncated: bool,
    /// Its final answer (capped for the parent's context; see
    /// `summary_ref`).
    pub summary: String,
    /// A file holding the full summary when `summary` was capped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_ref: Option<String>,
    /// The parsed answer when an `output_schema` was given.
    pub output: Option<Value>,
    /// Whether the answer matched the `output_schema` (`null` without
    /// one).
    pub schema_valid: Option<bool>,
    /// Why it did not.
    pub schema_errors: Vec<String>,
    /// Files written.
    pub artifacts: Vec<Artifact>,
    /// Usage.
    pub usage: ResultUsage,
    /// Cost in USD (`null` when a call was unpriced).
    pub cost_usd: Option<f64>,
    /// `exact`, `estimated`, `subscription` or `unknown`.
    pub cost_status: String,
    /// Wall time.
    pub duration_ms: u64,
    /// `provider/model` it ran on.
    pub model: String,
    /// The failure, when it failed.
    pub error: Option<ResultError>,
    /// Its session (the transcript and trajectory are found by it).
    pub trajectory_ref: Option<String>,
    /// Steer messages that never reached it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missed_steer: Vec<String>,
    /// Processes that did not stop within 10 seconds of its end.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub orphaned_processes: Vec<String>,
    /// Seconds without progress before a stall stopped it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quiet_secs: Option<u64>,
    /// Its isolation worktree (`delegation.worktree_isolation`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<WorktreeReport>,
    /// Why it ran differently than configured (isolation skipped).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl SubagentResult {
    /// A result for a child that never ran.
    #[must_use]
    pub fn refused(subagent_id: &str, task_index: usize, code: &str, message: &str) -> Self {
        Self::ended(
            subagent_id,
            task_index,
            Status::Refused,
            ExitReason::Error,
            Some(ResultError {
                code: code.to_owned(),
                message: message.to_owned(),
            }),
        )
    }

    /// A result with nothing but its outcome (no answer, no usage).
    #[must_use]
    pub fn ended(
        subagent_id: &str,
        task_index: usize,
        status: Status,
        exit_reason: ExitReason,
        error: Option<ResultError>,
    ) -> Self {
        Self {
            contract_version: CONTRACT_VERSION,
            subagent_id: subagent_id.to_owned(),
            task_index,
            status,
            exit_reason,
            truncated: false,
            summary: String::new(),
            summary_ref: None,
            output: None,
            schema_valid: None,
            schema_errors: Vec::new(),
            artifacts: Vec::new(),
            usage: ResultUsage::default(),
            cost_usd: None,
            cost_status: "unknown".to_owned(),
            duration_ms: 0,
            model: String::new(),
            error,
            trajectory_ref: None,
            missed_steer: Vec::new(),
            orphaned_processes: Vec::new(),
            quiet_secs: None,
            worktree: None,
            note: None,
        }
    }
}

/// The JSON Schema of [`SubagentResult`].
#[must_use]
pub fn schema() -> Value {
    serde_json::to_value(schemars::schema_for!(SubagentResult)).unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_schema_names_every_field_of_the_contract() {
        let schema = schema();
        let properties = schema["properties"].as_object().unwrap();
        for field in [
            "subagent_id",
            "task_index",
            "status",
            "exit_reason",
            "truncated",
            "summary",
            "output",
            "schema_valid",
            "schema_errors",
            "artifacts",
            "usage",
            "cost_usd",
            "cost_status",
            "duration_ms",
            "model",
            "error",
            "trajectory_ref",
        ] {
            assert!(properties.contains_key(field), "{field}");
        }
    }
}

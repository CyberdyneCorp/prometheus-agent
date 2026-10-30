//! Payloads of the session, turn, model, tool and delegation events.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Failure, Tokens, Usage};

/// `session.start`. The surface appears only here, as `source`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionStart {
    /// The surface (`cli`, `tui`, `gateway`, `cron`...).
    pub source: String,
    /// The session's title, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The model the session starts with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The parent session, for a continuation, fork or delegation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
}

/// `session.end`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionEnd {
    /// The end reason (`user_exit`, `compression`, `error`...).
    pub reason: String,
}

/// `session.tools_rebuilt`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ToolsRebuilt {
    /// What triggered the rebuild (`delegation --now`, `tools --now`...).
    pub reason: String,
    /// The tool names now offered.
    pub tools: Vec<String>,
}

/// `turn.start`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TurnStart {
    /// The user message that started the turn, when one did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
}

/// One tool call as summarised in `turn.step`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StepCall {
    /// Tool name.
    pub tool: String,
    /// SHA-256 of the canonical arguments.
    pub args_digest: String,
    /// Wall time.
    pub duration_ms: u64,
    /// `ok`, `error`, `denied` or `cancelled`.
    pub outcome: String,
}

/// `turn.step`: after each loop iteration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TurnStep {
    /// Iteration number from 1.
    pub iteration: u32,
    /// The iteration's tool calls.
    pub calls: Vec<StepCall>,
    /// Usage of the turn so far.
    pub usage: Usage,
}

/// `turn.end`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TurnEnd {
    /// The turn's end reason (`completed`, `max_iterations`, `interrupted`...).
    pub reason: String,
    /// Loop iterations run.
    pub iterations: u32,
    /// Usage of the whole turn.
    pub usage: Usage,
}

/// `user.message` and `assistant.message`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MessageText {
    /// The stored message, when it is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    /// The text.
    pub text: String,
}

/// `assistant.reasoning`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Reasoning {
    /// The stored message the reasoning belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    /// Readable reasoning text, when the provider returned any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Provider reasoning items (opaque, replayed verbatim).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub items: Option<Value>,
}

/// `model.call`: one main-loop model call, mirroring its ledger row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelCall {
    /// The ledger's call id.
    pub call_id: String,
    /// Provider name.
    pub provider: String,
    /// Model id.
    pub model: String,
    /// Why the call was made (`turn`...).
    pub purpose: String,
    /// Hash of the canonical request.
    pub request_hash: String,
    /// Whether the response came from the response cache.
    pub cache_hit: bool,
    /// Latency to the complete response.
    pub latency_ms: u64,
    /// Tokens by class.
    pub tokens: Tokens,
    /// Cost in USD, `null` when unpriced.
    pub cost_usd: Option<f64>,
    /// How the cost was obtained (`provider`, `pricing_table`, `unpriced`).
    pub cost_status: String,
    /// The billing source of the credential used.
    pub billing_source: String,
}

/// `aux_call.start`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AuxCallStart {
    /// The ledger's call id.
    pub call_id: String,
    /// The auxiliary task (`compression`, `title`, `vision`...).
    pub purpose: String,
    /// Provider name.
    pub provider: String,
    /// Model id.
    pub model: String,
    /// Attempt number from 1.
    pub attempt: u32,
}

/// `aux_call.end`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AuxCallEnd {
    /// The ledger's call id.
    pub call_id: String,
    /// Whether the attempt succeeded.
    pub ok: bool,
    /// Latency of the attempt.
    pub latency_ms: u64,
    /// Tokens by class.
    pub tokens: Tokens,
    /// Cost in USD, `null` when unpriced.
    pub cost_usd: Option<f64>,
    /// The failure, when `ok` is false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Failure>,
}

/// `tool.call`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ToolCall {
    /// The model's call id.
    pub call_id: String,
    /// Tool name.
    pub tool: String,
    /// Arguments as the model sent them (an object, or a raw string the
    /// model produced when it was not valid JSON).
    pub arguments: Value,
}

/// `tool.result`: either `output` or `error`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ToolResult {
    /// The call this answers.
    pub call_id: String,
    /// Tool name.
    pub tool: String,
    /// Whether the call succeeded.
    pub ok: bool,
    /// Wall time; `null` when unknown (a trace rebuilt from the transcript).
    pub duration_ms: Option<u64>,
    /// Output text, when `ok`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// The failure, when not `ok`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Failure>,
}

/// One dependency edge of a planned batch: `call` waits for `after`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BatchEdge {
    /// The waiting call.
    pub call: String,
    /// The call it waits for.
    pub after: String,
}

/// `tool.batch_planned`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BatchPlanned {
    /// Call ids in the batch.
    pub calls: Vec<String>,
    /// Dependency edges.
    pub edges: Vec<BatchEdge>,
}

/// `approval.request`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ApprovalRequest {
    /// The request id.
    pub request_id: String,
    /// The tool call needing approval.
    pub call_id: String,
    /// Tool name.
    pub tool: String,
    /// What would run (command line, path...).
    pub summary: String,
}

/// `approval.decision`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ApprovalDecision {
    /// The request answered.
    pub request_id: String,
    /// `once`, `session`, `always`, `deny` or `timeout`.
    pub decision: String,
    /// Who decided (`user`, `policy`, `timeout`).
    pub by: String,
}

/// `clarify.request`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ClarifyRequest {
    /// The request id.
    pub request_id: String,
    /// The question.
    pub question: String,
    /// Offered choices, if any.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<String>,
}

/// `clarify.answer`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ClarifyAnswer {
    /// The request answered.
    pub request_id: String,
    /// The answer, `null` on timeout.
    pub answer: Option<String>,
}

/// `subagent.spawned`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SubagentSpawned {
    /// The child's id.
    pub subagent_id: String,
    /// The child's session.
    pub child_session_id: String,
    /// Delegation depth from 1.
    pub depth: u32,
    /// The delegated goal.
    pub goal: String,
}

/// `subagent.progress`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SubagentProgress {
    /// The child's id.
    pub subagent_id: String,
    /// The parent's id (session or subagent).
    pub parent_id: String,
    /// Delegation depth from 1.
    pub depth: u32,
    /// The child's loop iteration.
    pub iteration: u32,
    /// The tool it is running, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// One-line activity summary (at most 120 characters).
    pub activity: String,
    /// Cumulative usage.
    pub usage: Usage,
}

/// `subagent.finished`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SubagentFinished {
    /// The child's id.
    pub subagent_id: String,
    /// `completed`, `failed`, `cancelled` or `timeout`.
    pub status: String,
    /// The child's summary.
    pub summary: String,
    /// Wall time.
    pub duration_ms: u64,
    /// Total usage.
    pub usage: Usage,
}

/// `compression.committed`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CompressionCommitted {
    /// `threshold`, `manual`, `overflow`...
    pub trigger: String,
    /// The compaction epoch.
    pub epoch: u32,
    /// Context tokens before.
    pub tokens_before: u64,
    /// Context tokens after.
    pub tokens_after: u64,
    /// Tokens of the summary.
    pub summary_tokens: u64,
    /// Preserved-reference coverage, 0 to 1, when checked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage: Option<f64>,
    /// Wall time.
    pub duration_ms: u64,
}

/// `compression.failed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CompressionFailed {
    /// `invalid_engine_output`, `lock_skipped`, `aux_failed`...
    pub reason: String,
    /// Detail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// `prompt.rebuilt`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PromptRebuilt {
    /// `new_session`, `compaction`, `model_changed`, `skills --now`...
    pub reason: String,
    /// Hash of the new system prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_hash: Option<String>,
}

/// `cache.unexpected_break`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CacheBreak {
    /// Canonical prefix hash expected (the previous request's).
    pub expected_prefix_hash: String,
    /// Prefix hash actually sent.
    pub actual_prefix_hash: String,
    /// Index of the first differing message, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_diff_index: Option<u32>,
}

/// `provider.fallback`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProviderFallback {
    /// Route given up (`provider/model`).
    pub from: String,
    /// Route now used.
    pub to: String,
    /// Why.
    pub reason: String,
    /// Whether the billing source changed (subscription vs API key).
    pub billing_change: bool,
}

/// `provider.route_selected`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RouteSelected {
    /// The routing policy.
    pub policy: String,
    /// The chosen route.
    pub selected: String,
    /// Rejected candidates with their reason.
    pub rejected: Vec<RejectedRoute>,
}

/// A candidate route `provider.route_selected` did not choose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RejectedRoute {
    /// The route.
    pub route: String,
    /// `health`, `capability`, `price`...
    pub reason: String,
}

/// `provider.registry_reloaded`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RegistryReloaded {
    /// Providers added, changed or removed.
    pub changed: Vec<String>,
}

/// `memory.recalled`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MemoryRecalled {
    /// Recalled entry ids.
    pub entry_ids: Vec<String>,
}

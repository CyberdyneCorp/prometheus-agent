//! What a turn returns (spec: agent-turn-loop, Turn entry contract; Typed
//! exit reasons).

use std::fmt;

use pmt_core::error::Error;
use pmt_core::id::MessageId;
use serde::{Deserialize, Serialize};

/// Why a turn ended: a closed set, so no code path can finish a turn with
/// a reason outside it (and deserializing an unlisted name fails).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitReason {
    /// The model gave a final response.
    Completed,
    /// The user stopped the turn.
    InterruptedByUser,
    /// The system stopped the turn (lost lease, shutdown).
    InterruptedBySystem,
    /// `agent.max_iterations` model calls were made.
    IterationLimit,
    /// A token budget would be exceeded.
    TokenBudgetExhausted,
    /// A USD budget would be exceeded (or the model is unpriced under one).
    CostBudgetExhausted,
    /// The wall-clock run budget ran out.
    RunBudgetExhausted,
    /// A loop guardrail stopped the turn.
    GuardrailStop,
    /// The model answered with neither text nor tool calls.
    EmptyResponse,
    /// The conversation no longer fits the context window.
    ContextExhausted,
    /// The provider failed and nothing could recover.
    ProviderFailed,
    /// Another process kept the session's turn lease.
    LeaseUnavailable,
    /// Assigned only when recovering a turn journal left unfinished.
    Crashed,
}

impl ExitReason {
    /// Every reason, in spec order.
    pub const ALL: [Self; 13] = [
        Self::Completed,
        Self::InterruptedByUser,
        Self::InterruptedBySystem,
        Self::IterationLimit,
        Self::TokenBudgetExhausted,
        Self::CostBudgetExhausted,
        Self::RunBudgetExhausted,
        Self::GuardrailStop,
        Self::EmptyResponse,
        Self::ContextExhausted,
        Self::ProviderFailed,
        Self::LeaseUnavailable,
        Self::Crashed,
    ];

    /// The wire spelling (`turn.end` reason, RPC results).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::InterruptedByUser => "interrupted_by_user",
            Self::InterruptedBySystem => "interrupted_by_system",
            Self::IterationLimit => "iteration_limit",
            Self::TokenBudgetExhausted => "token_budget_exhausted",
            Self::CostBudgetExhausted => "cost_budget_exhausted",
            Self::RunBudgetExhausted => "run_budget_exhausted",
            Self::GuardrailStop => "guardrail_stop",
            Self::EmptyResponse => "empty_response",
            Self::ContextExhausted => "context_exhausted",
            Self::ProviderFailed => "provider_failed",
            Self::LeaseUnavailable => "lease_unavailable",
            Self::Crashed => "crashed",
        }
    }

    /// Whether the summary handoff runs before the turn ends (a budget ran
    /// out before a final response).
    #[must_use]
    pub const fn hands_off(self) -> bool {
        matches!(
            self,
            Self::IterationLimit | Self::TokenBudgetExhausted | Self::CostBudgetExhausted
        )
    }
}

impl fmt::Display for ExitReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A turn id: a `UUIDv7` (time-ordered, random tail).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TurnId(String);

impl TurnId {
    /// A fresh id.
    #[must_use]
    pub fn new() -> Self {
        let mut bytes = [0u8; 16];
        if getrandom::fill(&mut bytes).is_err() {
            // No OS randomness: a time-ordered id is still unique enough
            // within one process for log correlation.
            bytes = u128::from(fastrand::u64(..)).to_be_bytes();
        }
        let ms = u64::try_from(jiff::Timestamp::now().as_millisecond()).unwrap_or(0);
        bytes[..6].copy_from_slice(&ms.to_be_bytes()[2..]);
        bytes[6] = (bytes[6] & 0x0f) | 0x70;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        let hex = crate::hex(&bytes);
        Self(format!(
            "{}-{}-{}-{}-{}",
            &hex[..8],
            &hex[8..12],
            &hex[12..16],
            &hex[16..20],
            &hex[20..]
        ))
    }

    /// The id text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for TurnId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for TurnId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Tokens and cost of a turn's model calls (the handoff request included).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct TurnUsage {
    /// Model calls made.
    pub calls: u32,
    /// Uncached input tokens.
    pub input_tokens: u64,
    /// Output tokens (reasoning included).
    pub output_tokens: u64,
    /// Input tokens read from the provider cache.
    pub cache_read_tokens: u64,
    /// Input tokens written to the provider cache.
    pub cache_write_tokens: u64,
    /// Reasoning tokens (part of `output_tokens`).
    pub reasoning_tokens: u64,
    /// Cost in USD; `None` when a call was unpriced.
    pub cost_usd: Option<f64>,
}

impl TurnUsage {
    /// Adds one call.
    pub fn add(&mut self, usage: &pmt_providers::event::Usage, cost: Option<f64>) {
        let first = self.calls == 0;
        self.calls += 1;
        self.input_tokens += usage.input_tokens;
        self.output_tokens += usage.output_tokens;
        self.cache_read_tokens += usage.cache_read_tokens;
        self.cache_write_tokens += usage.cache_write_tokens;
        self.reasoning_tokens += usage.reasoning_tokens;
        self.cost_usd = match (first, self.cost_usd, cost) {
            (true, _, cost) => cost,
            (false, Some(a), Some(b)) => Some(a + b),
            _ => None,
        };
    }
}

/// The outcome of [`run_turn`](super::Agent::run_turn).
#[derive(Debug, Serialize)]
pub struct TurnResult {
    /// This turn's id.
    pub turn_id: TurnId,
    /// The final answer (the summary handoff's text for a budget exit).
    pub final_response: Option<String>,
    /// Why the turn ended.
    pub exit_reason: ExitReason,
    /// Rows the turn added to the transcript, in order.
    pub messages_appended: Vec<MessageId>,
    /// Usage of the turn's model calls.
    pub usage: TurnUsage,
    /// Model calls counted against the iteration budget.
    pub iterations: u32,
    /// Who stopped an interrupted turn (`user`, `lease_lost`...).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interrupt_issuer: Option<String>,
    /// Notices for the user (context reference warnings, budget warnings).
    pub notices: Vec<String>,
    /// The failure behind a non-`completed` exit, when there is one.
    #[serde(skip)]
    pub error: Option<Error>,
}

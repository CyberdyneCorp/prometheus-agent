//! Turn loop, prompt, compression and guardrail sections (specs:
//! agent-turn-loop, context-compression, web-and-media-tools).

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The `[agent]` section.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "next_session"))]
pub struct AgentConfig {
    /// Maximum model calls per turn; `0` means unlimited.
    #[schemars(range(min = 0, max = 100_000))]
    pub max_iterations: u32,
    /// Tool calls of one batch that may run at once.
    #[schemars(range(min = 1, max = 64))]
    pub tool_concurrency: u32,
    /// Attempts per model call before the fallback chain is consulted.
    #[schemars(range(min = 1, max = 10))]
    pub api_max_retries: u32,
    /// Extra wait-and-retry cycles after retries and fallbacks are spent on
    /// a transient failure with nothing streamed; `0` disables.
    pub auto_recovery_cycles: u32,
    /// How long the `clarify` tool waits for an answer.
    pub clarify_timeout_secs: u64,
    /// Optional per-turn wall-clock budget; unset means no budget.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_budget_secs: Option<u64>,
    /// Selected personality overlay (`none`, a built-in name or a key of
    /// `[agent.personalities]`).
    pub personality: String,
    /// Custom or overriding personalities: name to prompt text.
    pub personalities: BTreeMap<String, String>,
    /// Reasoning effort; unset means `medium`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningLevel>,
    /// Per-model reasoning overrides: model id to a level or a tier name of
    /// `[agent.reasoning_tiers]`.
    pub reasoning_overrides: BTreeMap<String, String>,
    /// Provider-specific tier names mapped to a level, for example
    /// `ultra = "max"`.
    pub reasoning_tiers: BTreeMap<String, String>,
    /// Nudge a model that announces an action without calling a tool:
    /// `"auto"` (Responses-wire models only), `true`, `false` or a list of
    /// model-name substrings.
    pub intent_continuation: IntentContinuation,
    /// Empty-response retry guard.
    pub empty_response_guard: EmptyResponseGuard,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_iterations: 500,
            tool_concurrency: 8,
            api_max_retries: 3,
            auto_recovery_cycles: 5,
            clarify_timeout_secs: 3600,
            run_budget_secs: None,
            personality: "none".to_owned(),
            personalities: BTreeMap::new(),
            reasoning_effort: None,
            reasoning_overrides: BTreeMap::new(),
            reasoning_tiers: BTreeMap::new(),
            intent_continuation: IntentContinuation::Mode(ContinuationMode::Auto),
            empty_response_guard: EmptyResponseGuard::default(),
        }
    }
}

/// Reasoning effort levels, in increasing order.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningLevel {
    /// No reasoning.
    None,
    /// Minimal reasoning.
    Minimal,
    /// Low effort.
    Low,
    /// Medium effort.
    Medium,
    /// High effort.
    High,
    /// Extra-high effort.
    Xhigh,
    /// Maximum effort.
    Max,
    /// Beyond maximum where a route supports it.
    Ultra,
}

impl ReasoningLevel {
    /// Every level, lowest first.
    pub const ALL: [Self; 8] = [
        Self::None,
        Self::Minimal,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::Xhigh,
        Self::Max,
        Self::Ultra,
    ];

    /// The config spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
            Self::Ultra => "ultra",
        }
    }

    /// The level spelled `text` (case-insensitive).
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|level| level.as_str().eq_ignore_ascii_case(text.trim()))
    }
}

/// `agent.intent_continuation`: a mode, a switch or model-name substrings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum IntentContinuation {
    /// `"auto"`.
    Mode(ContinuationMode),
    /// Always on or always off.
    Enabled(bool),
    /// On for models whose name contains one of these substrings.
    Models(Vec<String>),
}

/// The named intent-continuation mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ContinuationMode {
    /// Responses-wire models only.
    Auto,
}

/// The `[agent.empty_response_guard]` section.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct EmptyResponseGuard {
    /// Estimated input cost (USD) of one empty attempt at which the retry
    /// budget drops to one.
    #[schemars(range(min = 0.0))]
    pub cost_threshold_usd: f64,
}

impl Default for EmptyResponseGuard {
    fn default() -> Self {
        Self {
            cost_threshold_usd: 0.25,
        }
    }
}

/// The `[prompt]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "next_session"))]
pub struct PromptConfig {
    /// Load the identity file `SOUL.md`.
    pub load_identity: bool,
    /// Load project context files (`AGENTS.md` and friends).
    pub load_context_files: bool,
    /// Character cap per context file; `0` derives it from the model's
    /// context window.
    pub context_file_max_chars: u64,
    /// Time allowed to read each context file.
    pub context_file_read_timeout_secs: u64,
    /// Add `Session ID: ses_<ULID>` to the session tier.
    pub include_session_id: bool,
    /// Per-platform hint overrides, keyed by platform.
    pub platform_hints: BTreeMap<String, PlatformHint>,
}

impl Default for PromptConfig {
    fn default() -> Self {
        Self {
            load_identity: true,
            load_context_files: true,
            context_file_max_chars: 0,
            context_file_read_timeout_secs: 5,
            include_session_id: false,
            platform_hints: BTreeMap::new(),
        }
    }
}

/// One `[prompt.platform_hints.<platform>]` entry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct PlatformHint {
    /// Replaces the built-in hint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replace: Option<String>,
    /// Appended after the (possibly replaced) hint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub append: Option<String>,
}

/// The `[prompt_cache]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "next_session"))]
pub struct PromptCacheConfig {
    /// Provider prompt-cache lifetime.
    pub ttl: PromptCacheTtl,
}

/// Prompt-cache lifetimes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum PromptCacheTtl {
    /// Five minutes.
    #[default]
    #[serde(rename = "5m")]
    FiveMinutes,
    /// One hour.
    #[serde(rename = "1h")]
    OneHour,
    /// Five minutes for machine-paced sessions (subagent, cron, webhook,
    /// kanban, api, batch), one hour otherwise; resolved once per session.
    #[serde(rename = "auto")]
    Auto,
    /// No cache markers.
    #[serde(rename = "off")]
    Off,
}

/// The `[context]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "next_session"))]
pub struct ContextConfig {
    /// Context engine: `builtin` or an installed `context-engine` plugin.
    pub engine: String,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            engine: "builtin".to_owned(),
        }
    }
}

/// The `[compression]` section.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "next_session"))]
#[allow(clippy::struct_excessive_bools)] // independent user switches
pub struct CompressionConfig {
    /// Compact the context automatically.
    pub enabled: bool,
    /// Fraction of the input budget at which compaction triggers.
    #[schemars(extend("exclusiveMinimum" = 0.0, "exclusiveMaximum" = 1.0))]
    pub threshold: f64,
    /// Lower absolute trigger in tokens; `0` is off.
    pub threshold_tokens: u64,
    /// Target size after compaction as a fraction of the input budget.
    #[schemars(range(min = 0.1, max = 0.8))]
    pub target_ratio: f64,
    /// How much recent conversation is kept verbatim.
    pub tail_mode: TailMode,
    /// Non-system messages protected at the head of the first compaction.
    pub protect_first_n: u32,
    /// User messages always kept in the tail.
    pub min_tail_user_messages: u32,
    /// Compaction attempts per turn.
    #[schemars(range(min = 1, max = 10))]
    pub max_attempts: u32,
    /// Summary length cap in tokens.
    pub summary_max_tokens: u64,
    /// End the turn when the summary cannot be produced.
    pub abort_on_summary_failure: bool,
    /// Minimum coverage a summary must reach.
    #[schemars(range(min = 0.0, max = 1.0))]
    pub quality_min_coverage: f64,
    /// Token cap for pinned content carried across compactions.
    pub pinned_max_tokens: u64,
    /// Compact tool results between turns.
    pub micro_compact: bool,
    /// Turns between micro-compactions.
    #[schemars(range(min = 1))]
    pub micro_compact_every_n_turns: u32,
    /// Prune old tool output above this many tokens; `0` is off.
    pub proactive_prune_tokens: u64,
    /// Prepare the next summary ahead of the trigger.
    pub prepare_ahead: bool,
    /// Time allowed for one summary call.
    pub summary_timeout_secs: u64,
}

impl Default for CompressionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            threshold: 0.50,
            threshold_tokens: 0,
            target_ratio: 0.20,
            tail_mode: TailMode::Lean,
            protect_first_n: 3,
            min_tail_user_messages: 1,
            max_attempts: 3,
            summary_max_tokens: 10_000,
            abort_on_summary_failure: false,
            quality_min_coverage: 0.8,
            pinned_max_tokens: 8000,
            micro_compact: false,
            micro_compact_every_n_turns: 1,
            proactive_prune_tokens: 0,
            prepare_ahead: false,
            summary_timeout_secs: 120,
        }
    }
}

/// Tail retention modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TailMode {
    /// Keep the minimum recent tail.
    Lean,
    /// Keep a tail sized by `target_ratio`.
    Ratio,
}

/// The `[tool_loop_guardrails]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "next_session"))]
pub struct ToolLoopGuardrails {
    /// Append a warning to the tool result when a `warn_after` threshold is
    /// reached.
    pub warnings_enabled: bool,
    /// Repeats that trigger a warning.
    pub warn_after: GuardrailThresholds,
    /// Stop the turn (`guardrail_stop`) at `hard_stop_after` on
    /// interactive surfaces.
    pub hard_stop_enabled: bool,
    /// Stop the turn at `hard_stop_after` on gateway, cron, API and kanban
    /// sessions.
    pub non_interactive_hard_stop_enabled: bool,
    /// Repeats that stop the turn.
    pub hard_stop_after: GuardrailThresholds,
    /// Per-turn call caps, always enforced.
    pub loop_caps: LoopCaps,
}

impl Default for ToolLoopGuardrails {
    fn default() -> Self {
        Self {
            warnings_enabled: true,
            warn_after: GuardrailThresholds {
                exact_failure: 2,
                same_tool_failure: 3,
                idempotent_no_progress: 2,
            },
            hard_stop_enabled: false,
            non_interactive_hard_stop_enabled: true,
            hard_stop_after: GuardrailThresholds {
                exact_failure: 5,
                same_tool_failure: 8,
                idempotent_no_progress: 5,
            },
            loop_caps: LoopCaps::default(),
        }
    }
}

/// Repetition thresholds of the tool loop guardrails.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GuardrailThresholds {
    /// Same tool, same arguments, same error.
    #[schemars(range(min = 1))]
    pub exact_failure: u32,
    /// Failures of the same tool.
    #[schemars(range(min = 1))]
    pub same_tool_failure: u32,
    /// Idempotent calls returning identical results.
    #[schemars(range(min = 1))]
    pub idempotent_no_progress: u32,
}

/// The `[tool_loop_guardrails.loop_caps]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct LoopCaps {
    /// Web searches per turn; `0` is unlimited.
    pub max_web_searches: u32,
    /// Subagents per turn; `0` is unlimited.
    pub max_subagents: u32,
}

impl Default for LoopCaps {
    fn default() -> Self {
        Self {
            max_web_searches: 50,
            max_subagents: 50,
        }
    }
}

/// The `[vision]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "next_session"))]
pub struct VisionConfig {
    /// How images reach the model.
    pub image_input_mode: ImageInputMode,
}

/// Image input routing modes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ImageInputMode {
    /// Pixels when the main route supports vision, else a description.
    #[default]
    Auto,
    /// Always attach pixels.
    Native,
    /// Never attach pixels; use a `vision` auxiliary description.
    Text,
}

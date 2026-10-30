//! The typed `config.toml` schema: the single source of every key, type,
//! default, allowed value, range and `apply` class (spec:
//! configuration-and-profiles, Typed configuration schema is the single
//! source of defaults). The JSON Schema, the configuration reference, the
//! validator, `config explain` and completions are all derived from these
//! types, so a key is defined exactly once.
//!
//! Conventions (design, Shared contracts): `snake_case` keys, durations end
//! in `_secs` or `_ms`, sizes in `_bytes`, closed value sets are enums.
//! Unknown keys are rejected everywhere (`deny_unknown_fields`). Each
//! section struct carries `x-apply` (`live`, `next_session` or `restart`):
//! when a changed value takes effect in a running process. A field can
//! override its section's class. Doc comments become schema descriptions,
//! so they are written for users.
//!
//! Each capability's spec owns its section; the M1 sections live here so
//! the whole file validates in one place.

mod agent;
mod interface;
mod models;
mod operations;
mod state;
mod tools;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub use agent::{
    AgentConfig, CompressionConfig, ContextConfig, ContinuationMode, EmptyResponseGuard,
    GuardrailThresholds, ImageInputMode, IntentContinuation, LoopCaps, PlatformHint,
    PromptCacheConfig, PromptCacheTtl, PromptConfig, ReasoningLevel, TailMode, ToolLoopGuardrails,
    VisionConfig,
};
pub use interface::{
    CliConfig, DisplayConfig, Interface, MarkdownMode, ResumeDisplay, ToolProgress,
    ToolProgressGrouping, UiConfig,
};
pub use models::{
    AuthConfig, AuxiliaryConfig, AuxiliaryTask, FallbackConfig, FallbackEntry, LoginFlow,
    ModelConfig, ModelEntry, ModelPrice, ModelRoute, PoolStrategy, PreferCredential, ProviderAuth,
    ProviderConfig, WireFormat,
};
pub use operations::{
    BudgetsConfig, CronConfig, DegradedFeature, EgressConfig, LogLevel, LoggingConfig,
    NetworkConfig, ObservabilityConfig, SandboxConfig, SandboxMode, SecretsBackend, SecretsConfig,
    SecurityConfig, TelemetryConfig, TrajectoriesConfig,
};
pub use state::{
    JournalMode, MemoryConfig, MemoryReviewMode, RecallConfig, SessionsConfig, SkillsConfig,
    SnapshotsConfig, StorageConfig,
};
pub use tools::{
    ApprovalMode, ApprovalsConfig, BrowserConfig, ContainerConfig, DegradedMode, DialogPolicy,
    FilesConfig, KeylessConfig, ResultBudget, SshConfig, TerminalConfig, ToolOutputConfig,
    ToolsConfig, ToolsetsConfig, UnattendedApproval, WebConfig, WindowsShell,
};

/// The `config.toml` schema version this build writes and reads.
pub const SCHEMA_VERSION: u32 = 1;

/// Root of a profile's `config.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(title = "Prometheus config.toml")]
pub struct Config {
    /// Version of this file's schema; older files are migrated forward and
    /// a newer one is refused.
    #[schemars(extend("x-apply" = "restart"))]
    pub schema_version: u32,
    /// Turn loop limits, retries and reasoning effort.
    pub agent: AgentConfig,
    /// Approval modes for dangerous tool calls.
    pub approvals: ApprovalsConfig,
    /// Sign-in behaviour.
    pub auth: AuthConfig,
    /// Side-model (auxiliary) task routing.
    pub auxiliary: AuxiliaryConfig,
    /// Browser tool sessions.
    pub browser: BrowserConfig,
    /// Hard token and cost budgets (`0` = off).
    pub budgets: BudgetsConfig,
    /// Line-mode CLI behaviour.
    pub cli: CliConfig,
    /// Context compression.
    pub compression: CompressionConfig,
    /// Context engine selection.
    pub context: ContextConfig,
    /// Profile time zone (scheduler keys arrive with M2).
    pub cron: CronConfig,
    /// Display preferences of interactive surfaces.
    pub display: DisplayConfig,
    /// Provider fallback.
    pub fallback: FallbackConfig,
    /// `read_file` limits.
    pub files: FilesConfig,
    /// Structured log files.
    pub logging: LoggingConfig,
    /// Built-in memory.
    pub memory: MemoryConfig,
    /// The main model and provider.
    pub model: ModelConfig,
    /// Model aliases and metadata overrides, keyed by name or
    /// `"<provider>/<model>"`.
    #[schemars(extend("x-apply" = "next_session"))]
    pub models: std::collections::BTreeMap<String, ModelEntry>,
    /// Outbound networking.
    pub network: NetworkConfig,
    /// Trajectories.
    pub observability: ObservabilityConfig,
    /// System prompt assembly.
    pub prompt: PromptConfig,
    /// Provider prompt caching.
    pub prompt_cache: PromptCacheConfig,
    /// Per-provider overrides, keyed by provider id.
    #[schemars(extend("x-apply" = "next_session"))]
    pub providers: std::collections::BTreeMap<String, ProviderConfig>,
    /// Secrets store backend.
    pub secrets: SecretsConfig,
    /// Capabilities, sandbox, egress and redaction.
    pub security: SecurityConfig,
    /// Session storage limits and retention.
    pub sessions: SessionsConfig,
    /// Skills discovery and authoring.
    pub skills: SkillsConfig,
    /// Per-profile state snapshots.
    pub snapshots: SnapshotsConfig,
    /// Session database.
    pub storage: StorageConfig,
    /// Opt-in telemetry.
    pub telemetry: TelemetryConfig,
    /// The terminal tool and its backends.
    pub terminal: TerminalConfig,
    /// Tool loop guardrails.
    pub tool_loop_guardrails: ToolLoopGuardrails,
    /// Terminal tool output shaping.
    pub tool_output: ToolOutputConfig,
    /// Toolset selection, timeouts and result budgets.
    pub tools: ToolsConfig,
    /// Per-surface toolset defaults.
    pub toolsets: ToolsetsConfig,
    /// Interactive interface selection.
    pub ui: UiConfig,
    /// Image input routing.
    pub vision: VisionConfig,
    /// Web tools.
    pub web: WebConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            agent: AgentConfig::default(),
            approvals: ApprovalsConfig::default(),
            auth: AuthConfig::default(),
            auxiliary: AuxiliaryConfig::default(),
            browser: BrowserConfig::default(),
            budgets: BudgetsConfig::default(),
            cli: CliConfig::default(),
            compression: CompressionConfig::default(),
            context: ContextConfig::default(),
            cron: CronConfig::default(),
            display: DisplayConfig::default(),
            fallback: FallbackConfig::default(),
            files: FilesConfig::default(),
            logging: LoggingConfig::default(),
            memory: MemoryConfig::default(),
            model: ModelConfig::default(),
            models: std::collections::BTreeMap::new(),
            network: NetworkConfig::default(),
            observability: ObservabilityConfig::default(),
            prompt: PromptConfig::default(),
            prompt_cache: PromptCacheConfig::default(),
            providers: std::collections::BTreeMap::new(),
            secrets: SecretsConfig::default(),
            security: SecurityConfig::default(),
            sessions: SessionsConfig::default(),
            skills: SkillsConfig::default(),
            snapshots: SnapshotsConfig::default(),
            storage: StorageConfig::default(),
            telemetry: TelemetryConfig::default(),
            terminal: TerminalConfig::default(),
            tool_loop_guardrails: ToolLoopGuardrails::default(),
            tool_output: ToolOutputConfig::default(),
            tools: ToolsConfig::default(),
            toolsets: ToolsetsConfig::default(),
            ui: UiConfig::default(),
            vision: VisionConfig::default(),
            web: WebConfig::default(),
        }
    }
}

/// Root sections that are machine-wide and live in `<home>/daemon.toml`;
/// a profile `config.toml` that contains one is refused (design D13).
pub const MACHINE_WIDE_SECTIONS: &[&str] =
    &["daemon", "local_runtime", "process", "runtimes", "update"];

/// The name of a secret in the secrets store (`^[A-Z][A-Z0-9_]{0,127}$`).
/// Config keys that need a credential hold a name, never the value.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
#[schemars(extend("x-secret-ref" = true, "pattern" = "^[A-Z][A-Z0-9_]{0,127}$"))]
pub struct SecretName(pub String);

/// A capability string of the closed vocabulary (`pmt_core::capability`);
/// validation refuses any other string with `unknown capability '<s>'`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
#[schemars(extend("x-capability" = true))]
pub struct CapabilityText(pub String);

/// Whether `name` is a valid secret name.
#[must_use]
pub fn is_secret_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_uppercase())
        && name.len() <= 128
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::{Config, is_secret_name};

    #[test]
    fn empty_file_is_every_default() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config, Config::default());
        assert_eq!(config.agent.max_iterations, 500);
    }

    #[test]
    fn defaults_round_trip_through_toml() {
        let text = toml::to_string(&Config::default()).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back, Config::default());
    }

    #[test]
    fn secret_names() {
        assert!(is_secret_name("OPENAI_API_KEY"));
        assert!(is_secret_name("A"));
        assert!(!is_secret_name("openai_api_key"));
        assert!(!is_secret_name("1KEY"));
        assert!(!is_secret_name(""));
        assert!(!is_secret_name(&"A".repeat(129)));
    }
}

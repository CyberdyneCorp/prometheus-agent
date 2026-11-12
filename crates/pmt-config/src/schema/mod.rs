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
mod api_server;
mod automation;
mod connectors;
mod cron;
mod curator;
mod delegation;
mod gateway;
mod gateway_display;
mod gateway_groups;
mod gateway_platform;
mod gateway_policy;
mod gateway_routes;
mod hooks;
mod interface;
mod lsp;
mod mcp;
mod media;
mod moa;
mod models;
mod operations;
mod plugins;
mod provider_routing;
mod python_bridge;
mod replay;
mod state;
mod tool_gateway;
mod tools;
mod voice;
mod webhook;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub use agent::{
    AgentConfig, AutoMode, CodingContext, CompressionConfig, ContextConfig, ContinuationMode,
    EmptyResponseGuard, GuardrailThresholds, ImageInputMode, IntentContinuation, LoopCaps,
    PlatformHint, PromptCacheConfig, PromptCacheTtl, PromptConfig, ReasoningLevel, ResumeOnCrash,
    TailMode, ToolLoopGuardrails, VerifyOnStop, VisionConfig,
};
pub use api_server::{ApiModelRoute, ApiResponsesConfig, ApiServerConfig};
pub use automation::{GoalsConfig, HeartbeatConfig, LoopsConfig};
pub use connectors::{ConnectorsConfig, DEFAULT_CONNECTOR_GATEWAY};
pub use cron::{ContinuableSurface, CronConfig, CronDeliveryConfig, CronProvider};
pub use curator::{
    CuratorConfig, DEFAULT_LEDGER_MAX_BYTES, DEFAULT_TRUSTED_SOURCES, SkillSignaturePolicy,
};
pub use delegation::{
    BackgroundMode, CODE_EXECUTION_TOOLS, ChildApproval, CodeExecutionConfig, CodeExecutionMode,
    DelegationConfig, GatewayDelegationMode, ReviewConfig,
};
pub use gateway::{
    AgentCacheConfig, AllowBots, BackgroundNotifications, Batching, ChatPolicy, ChatToolProgress,
    DedupConfig, DeliveryConfig, EffectiveBots, EffectiveBusy, EffectiveDisplay, EffectiveGroups,
    GatewayBackground, GatewayBots, GatewayBusy, GatewayConfig, GatewayDisplay, GatewayGroups,
    GatewayQuickCommands, GatewaySessions, HomeChannel, McpEvents, MediaConfig, MessageTimestamps,
    NotificationsConfig, OutboxConfig, PhraseMode, PlatformBots, PlatformBusy, PlatformConfig,
    PlatformDisplay, PlatformGroups, PlatformSessions, PlatformStreaming, PolicyChatType,
    PolicyFacts, RateLimitConfig, RateLimitOverride, ReplyToMode, ShutdownConfig, StatusPhrases,
    StreamTransport, StreamingConfig, UnauthorizedDm, VoiceConfig, VoiceReplyMode, WebhookRoute,
    resolve_policy,
};
pub use gateway_routes::{ProfileRoute, RouteFacts, resolve_route};
pub use hooks::{HooksConfig, OutboundHook, ShellHook};
pub use interface::{
    AutoResume, BusyConfig, BusyIndicator, BusyInputMode, CliConfig, ComposerConfig, DisplayConfig,
    Interface, KeyChord, MarkdownMode, QuickCommand, QuickCommandKind, ResumeDisplay, ToolProgress,
    ToolProgressGrouping, TuiConfig, TuiKeys, TuiTheme, UiConfig,
};
pub use lsp::{
    LSP_SERVER_IDS, LspConfig, LspInstallStrategy, LspPackageManager, LspServerConfig, LspWaitMode,
};
pub use mcp::{
    McpAuth, McpCapabilities, McpConfig, McpElicitationConfig, McpOAuthConfig, McpOAuthFlow,
    McpRedirectHost, McpSamplingConfig, McpServerConfig, McpServerModeConfig, McpToolFilter,
    McpTransport, McpTrust,
};
pub use media::{
    CommandSttConfig, CommandTtsConfig, EdgeTtsConfig, ElevenLabsSttConfig, ElevenLabsTtsConfig,
    GeminiTtsConfig, ImageGenConfig, ImageOpenAiConfig, LocalSttConfig, OpenAiTtsConfig,
    SpeechProviderType, SttConfig, SttVendorConfig, TranscriptFormat, TtsConfig, TtsOutputFormat,
    VoiceModelConfig, WhisperCppConfig, WhisperModel, XSearchConfig, XaiTtsConfig,
};
pub use moa::{
    DEFAULT_PRESET, MoaConfig, MoaFanout, MoaPreset, MoaPrivacyFilter, MoaSlot, TRACE_DIR,
};
pub use models::{
    AliasRoute, AuthConfig, AuxiliaryConfig, AuxiliaryTask, FallbackConfig, FallbackEntry,
    LoginFlow, ModelAlias, ModelConfig, ModelEntry, ModelPrice, ModelRoute, OAuthEndpoints,
    PoolStrategy, PreferCredential, ProviderAuth, ProviderConfig, RoutingConfig, WireFormat,
};
pub use operations::{
    BudgetsConfig, DebugConfig, DebugShareConfig, DegradedFeature, EgressConfig, EgressMode,
    GatewayHealthConfig, HealthListenerConfig, LogLevel, LoggingConfig, MetricsEndpointConfig,
    NetworkConfig, ObservabilityConfig, OtelConfig, OtelProtocol, PrivacyConfig, SandboxConfig,
    SandboxMode, SecretCommandConfig, SecretsBackend, SecretsConfig, SecurityConfig,
    TelemetryConfig, TrajectoriesConfig, VaultConfig, VaultKeyStorage, VaultSourceConfig,
    WorktreesConfig,
};
pub use plugins::{
    DEFAULT_CATALOG_URL, GuidanceMode, PluginsConfig, PluginsWasmConfig, SecurityGuidanceConfig,
    SignaturePolicy, TransportFallback,
};
pub use provider_routing::{
    DEFAULT_MIN_CODING_SCORE, DataCollection, FastMode, OpenRouterConfig, ProviderRoutingConfig,
    RoutingPrefs, RoutingSort,
};
pub use python_bridge::{PYTHON_VERSIONS, PythonBridgeConfig, PythonVersion};
pub use replay::{CacheMode, DEFAULT_NAMESPACE, ModelCallCacheConfig, ReplayConfig};
pub use state::{
    AttachmentStorage, AttachmentsConfig, JournalMode, MemoryConfig, MemoryNotifications,
    MemoryReviewConfig, MemoryReviewMode, RecallConfig, ReviewDefer, SessionsConfig, SkillsConfig,
    SnapshotsConfig, StorageConfig,
};
pub use tool_gateway::{ToolGatewayConfig, ToolProviderConfig};
pub use tools::{
    ApprovalMode, ApprovalsConfig, BrowserConfig, ContainerConfig, DegradedMode, DialogPolicy,
    FilesConfig, FirecrawlConfig, KeylessConfig, ModalConfig, ModalMode, ProviderTier,
    ResultBudget, SearxngConfig, SingularityConfig, SnapshotConfig, SnapshotTerminal, SshConfig,
    SurfaceToolsets, TerminalConfig, ToolListing, ToolOutputConfig, ToolSearchConfig,
    ToolSearchMode, ToolsConfig, ToolsetsConfig, UnattendedApproval, WebConfig, WebXaiConfig,
    WindowsShell,
};
pub use voice::VoiceModeConfig;
pub use webhook::{
    FilterFile, FilterPath, FilterPattern, FilterValue, FilterValues, WebhookFilter, WebhookFilters,
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
    /// The OpenAI-compatible API server's view of this profile.
    pub api_server: ApiServerConfig,
    /// Approval modes for dangerous tool calls.
    pub approvals: ApprovalsConfig,
    /// Where received attachments are kept.
    pub attachments: AttachmentsConfig,
    /// Sign-in behaviour.
    pub auth: AuthConfig,
    /// Side-model (auxiliary) task routing.
    pub auxiliary: AuxiliaryConfig,
    /// Browser tool sessions.
    pub browser: BrowserConfig,
    /// Hard token and cost budgets (`0` = off).
    pub budgets: BudgetsConfig,
    /// What input typed while a turn runs does.
    pub busy: BusyConfig,
    /// Line-mode CLI behaviour.
    pub cli: CliConfig,
    /// Context compression.
    pub compression: CompressionConfig,
    /// Programmatic tool calling (`execute_code`).
    pub code_execution: CodeExecutionConfig,
    /// Hosted app connectors (Gmail, Notion...) through the Nous gateway.
    pub connectors: ConnectorsConfig,
    /// Context engine selection.
    pub context: ContextConfig,
    /// Profile time zone (scheduler keys arrive with M2).
    pub cron: CronConfig,
    /// The skill curator.
    pub curator: CuratorConfig,
    /// Subagent delegation.
    pub delegation: DelegationConfig,
    /// Diagnostics: the debug share target.
    pub debug: DebugConfig,
    /// Display preferences of interactive surfaces.
    pub display: DisplayConfig,
    /// Provider fallback.
    pub fallback: FallbackConfig,
    /// `read_file` limits.
    pub files: FilesConfig,
    /// The profile's part in the home's gateway daemon.
    pub gateway: GatewayConfig,
    /// `/goal`: the judge loop's turn budget, quality gates and waits.
    pub goals: GoalsConfig,
    /// `/heartbeat` bounds.
    pub heartbeat: HeartbeatConfig,
    /// Image generation (`image_generate`).
    pub image_gen: ImageGenConfig,
    /// Shell hooks and outbound webhooks.
    pub hooks: HooksConfig,
    /// Structured log files.
    pub logging: LoggingConfig,
    /// `/loop` cadence bounds.
    pub loops: LoopsConfig,
    /// Language server diagnostics after file edits.
    pub lsp: LspConfig,
    /// External MCP servers.
    pub mcp: McpConfig,
    /// What `prometheus mcp serve` exposes.
    pub mcp_server: McpServerModeConfig,
    /// Built-in memory.
    pub memory: MemoryConfig,
    /// Mixture of Agents presets.
    pub moa: MoaConfig,
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
    /// OpenRouter-only request settings.
    pub openrouter: OpenRouterConfig,
    /// Plugin enablement, hook bounds and WASM limits.
    pub plugins: PluginsConfig,
    /// System prompt assembly.
    pub prompt: PromptConfig,
    /// The python bridge: managed interpreter and sidecar bounds.
    pub python_bridge: PythonBridgeConfig,
    /// Provider prompt caching.
    pub prompt_cache: PromptCacheConfig,
    /// PII redaction.
    pub privacy: PrivacyConfig,
    /// OpenRouter upstream provider routing preferences.
    pub provider_routing: ProviderRoutingConfig,
    /// Per-provider overrides, keyed by provider id.
    #[schemars(extend("x-apply" = "next_session"))]
    pub providers: std::collections::BTreeMap<String, ProviderConfig>,
    /// User slash commands (`/<name>`), keyed by name.
    #[schemars(extend("x-apply" = "live"))]
    pub quick_commands: std::collections::BTreeMap<String, QuickCommand>,
    /// The model-call cache.
    pub replay: ReplayConfig,
    /// Provider health scoring.
    pub routing: RoutingConfig,
    /// Secrets store backend.
    pub secrets: SecretsConfig,
    /// Capabilities, sandbox, egress and redaction.
    pub security: SecurityConfig,
    /// Session storage limits and retention.
    pub sessions: SessionsConfig,
    /// Speech-to-text providers.
    pub stt: SttConfig,
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
    /// The subscription tool gateway.
    pub tool_gateway: ToolGatewayConfig,
    /// Tool loop guardrails.
    pub tool_loop_guardrails: ToolLoopGuardrails,
    /// Terminal tool output shaping.
    pub tool_output: ToolOutputConfig,
    /// Toolset selection, timeouts and result budgets.
    pub tools: ToolsConfig,
    /// Per-surface toolset defaults.
    pub toolsets: ToolsetsConfig,
    /// Text-to-speech (`text_to_speech`) providers.
    pub tts: TtsConfig,
    /// Interactive interface selection.
    pub ui: UiConfig,
    /// The credential vault.
    pub vault: VaultConfig,
    /// Voice conversations on the CLI, TUI and desktop.
    pub voice: VoiceModeConfig,
    /// Video generation's provider selection.
    pub video_gen: ToolProviderConfig,
    /// Image input routing.
    pub vision: VisionConfig,
    /// Web tools.
    pub web: WebConfig,
    /// The `x_search` tool.
    pub x_search: XSearchConfig,
    /// Git worktrees: creation, the stale-tree sweep and archives.
    pub worktrees: WorktreesConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            agent: AgentConfig::default(),
            api_server: ApiServerConfig::default(),
            approvals: ApprovalsConfig::default(),
            attachments: AttachmentsConfig::default(),
            auth: AuthConfig::default(),
            auxiliary: AuxiliaryConfig::default(),
            browser: BrowserConfig::default(),
            budgets: BudgetsConfig::default(),
            busy: BusyConfig::default(),
            cli: CliConfig::default(),
            compression: CompressionConfig::default(),
            code_execution: CodeExecutionConfig::default(),
            connectors: ConnectorsConfig::default(),
            context: ContextConfig::default(),
            cron: CronConfig::default(),
            curator: CuratorConfig::default(),
            delegation: DelegationConfig::default(),
            debug: DebugConfig::default(),
            display: DisplayConfig::default(),
            fallback: FallbackConfig::default(),
            files: FilesConfig::default(),
            gateway: GatewayConfig::default(),
            goals: GoalsConfig::default(),
            heartbeat: HeartbeatConfig::default(),
            image_gen: ImageGenConfig::default(),
            hooks: HooksConfig::default(),
            logging: LoggingConfig::default(),
            loops: LoopsConfig::default(),
            lsp: LspConfig::default(),
            mcp: McpConfig::default(),
            mcp_server: McpServerModeConfig::default(),
            memory: MemoryConfig::default(),
            moa: MoaConfig::default(),
            model: ModelConfig::default(),
            models: std::collections::BTreeMap::new(),
            network: NetworkConfig::default(),
            observability: ObservabilityConfig::default(),
            openrouter: OpenRouterConfig::default(),
            plugins: PluginsConfig::default(),
            prompt: PromptConfig::default(),
            python_bridge: PythonBridgeConfig::default(),
            prompt_cache: PromptCacheConfig::default(),
            privacy: PrivacyConfig::default(),
            provider_routing: ProviderRoutingConfig::default(),
            providers: std::collections::BTreeMap::new(),
            quick_commands: std::collections::BTreeMap::new(),
            replay: ReplayConfig::default(),
            routing: RoutingConfig::default(),
            secrets: SecretsConfig::default(),
            security: SecurityConfig::default(),
            sessions: SessionsConfig::default(),
            stt: SttConfig::default(),
            skills: SkillsConfig::default(),
            snapshots: SnapshotsConfig::default(),
            storage: StorageConfig::default(),
            telemetry: TelemetryConfig::default(),
            terminal: TerminalConfig::default(),
            tool_gateway: ToolGatewayConfig::default(),
            tool_loop_guardrails: ToolLoopGuardrails::default(),
            tool_output: ToolOutputConfig::default(),
            tools: ToolsConfig::default(),
            toolsets: ToolsetsConfig::default(),
            tts: TtsConfig::default(),
            ui: UiConfig::default(),
            vault: VaultConfig::default(),
            voice: VoiceModeConfig::default(),
            video_gen: ToolProviderConfig::default(),
            vision: VisionConfig::default(),
            web: WebConfig::default(),
            x_search: XSearchConfig::default(),
            worktrees: WorktreesConfig::default(),
        }
    }
}

/// Root sections that are machine-wide and live in `<home>/daemon.toml`;
/// a profile `config.toml` that contains one is refused (design D13).
pub const MACHINE_WIDE_SECTIONS: &[&str] =
    &["daemon", "local_runtime", "process", "runtimes", "update"];

/// `[gateway]` keys that are daemon-wide and live in `daemon.toml`.
pub const MACHINE_WIDE_GATEWAY_KEYS: &[&str] = &["http", "relay", "on_all_adapters_down"];

/// `[api_server]` keys that are daemon-wide and live in `daemon.toml`.
pub const MACHINE_WIDE_API_KEYS: &[&str] = &["listen", "public", "cors_origins", "public_host"];

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

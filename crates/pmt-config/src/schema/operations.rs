//! Observability, budgets, security, secrets and network sections (specs:
//! observability, security-boundaries, configuration-and-profiles,
//! workspace-architecture).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The `[observability]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "next_session"))]
pub struct ObservabilityConfig {
    /// Session trajectories.
    pub trajectories: TrajectoriesConfig,
}

/// The `[observability.trajectories]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct TrajectoriesConfig {
    /// Record trajectories for sessions started from now on.
    pub enabled: bool,
    /// Delete unpinned trajectories older than this; `0` keeps forever.
    pub retention_secs: u64,
    /// Stop recording payload bodies of a session beyond this size.
    pub max_bytes_per_session: u64,
}

impl Default for TrajectoriesConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            retention_secs: 7_776_000,
            max_bytes_per_session: 256 * 1024 * 1024,
        }
    }
}

/// The `[logging]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "restart"))]
pub struct LoggingConfig {
    /// Log level; `PMT_LOG` or `RUST_LOG` override it.
    pub level: LogLevel,
}

/// Log levels.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    /// Errors only.
    Error,
    /// Warnings and errors.
    Warn,
    /// Informational records.
    #[default]
    Info,
    /// Debug detail.
    Debug,
    /// Everything.
    Trace,
}

/// The `[budgets]` section. Every limit defaults to `0` (off) except
/// `unattended_root_usd`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "live"))]
pub struct BudgetsConfig {
    /// Tokens per turn.
    pub turn_tokens: u64,
    /// USD per turn.
    #[schemars(range(min = 0.0))]
    pub turn_usd: f64,
    /// Tokens per session.
    pub session_tokens: u64,
    /// USD per session.
    #[schemars(range(min = 0.0))]
    pub session_usd: f64,
    /// Tokens per root session lineage.
    pub root_tokens: u64,
    /// USD per root session lineage.
    #[schemars(range(min = 0.0))]
    pub root_usd: f64,
    /// USD per day.
    #[schemars(range(min = 0.0))]
    pub daily_usd: f64,
    /// USD per month.
    #[schemars(range(min = 0.0))]
    pub monthly_usd: f64,
    /// USD per root lineage of cron, unattended gateway sessions,
    /// background delegations and harness evolution.
    #[schemars(range(min = 0.0))]
    pub unattended_root_usd: f64,
    /// Fraction of a limit at which a budget warning is emitted.
    #[schemars(range(min = 0.0, max = 1.0))]
    pub warn_ratio: f64,
    /// Allow models without a known price under a USD budget (token
    /// budgets still apply).
    pub allow_unpriced: bool,
}

impl Default for BudgetsConfig {
    fn default() -> Self {
        Self {
            turn_tokens: 0,
            turn_usd: 0.0,
            session_tokens: 0,
            session_usd: 0.0,
            root_tokens: 0,
            root_usd: 0.0,
            daily_usd: 0.0,
            monthly_usd: 0.0,
            unattended_root_usd: 5.0,
            warn_ratio: 0.8,
            allow_unpriced: false,
        }
    }
}

/// The `[telemetry]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "restart"))]
pub struct TelemetryConfig {
    /// Send redacted aggregate counters.
    pub enabled: bool,
    /// HTTPS endpoint (loopback HTTP allowed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
}

/// The `[secrets]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "restart"))]
pub struct SecretsConfig {
    /// Where this profile's secrets are stored.
    pub backend: SecretsBackend,
}

/// Secrets store backends.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SecretsBackend {
    /// `<profile>/secrets.toml`, mode 0600.
    #[default]
    File,
    /// The OS keychain.
    Keychain,
}

/// The `[security]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "next_session"))]
pub struct SecurityConfig {
    /// Redact every loaded secret value everywhere output is written.
    pub redact_secrets: bool,
    /// Profile capability baseline; unset uses the built-in baseline
    /// (workspace read/write, toolchain read, `process.spawn:*`, egress
    /// policy, `llm.call`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Vec<String>>,
    /// OS sandbox.
    pub sandbox: SandboxConfig,
    /// Outbound network policy.
    pub egress: EgressConfig,
    /// Require human approval for agent writes to instruction files.
    pub protected_instruction_files: bool,
    /// Extra instruction-file basename globs.
    pub protected_instruction_extra_patterns: Vec<String>,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            redact_secrets: true,
            capabilities: None,
            sandbox: SandboxConfig::default(),
            egress: EgressConfig::default(),
            protected_instruction_files: true,
            protected_instruction_extra_patterns: Vec::new(),
        }
    }
}

/// The `[security.sandbox]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SandboxConfig {
    /// Sandbox mode; `off` needs `allow_degraded` to name `all`.
    pub mode: SandboxMode,
    /// Hardening features that may be missing; anything else missing
    /// refuses to run.
    pub allow_degraded: Vec<DegradedFeature>,
}

/// Sandbox modes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SandboxMode {
    /// Full OS enforcement.
    #[default]
    Enforce,
    /// No OS sandbox (a downgrade).
    Off,
}

/// Sandbox hardening features.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DegradedFeature {
    /// Linux Landlock filesystem rules.
    Landlock,
    /// Linux Landlock network rules.
    LandlockNet,
    /// Linux seccomp filter.
    Seccomp,
    /// macOS Seatbelt.
    Seatbelt,
    /// Windows `AppContainer`.
    Appcontainer,
    /// Container `no-new-privileges`.
    NoNewPrivileges,
    /// Container read-only root.
    ReadOnlyRootfs,
    /// Container `--cap-drop ALL`.
    CapDrop,
    /// Every feature.
    All,
}

/// The `[security.egress]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct EgressConfig {
    /// Allow private destinations without a session grant.
    pub allow_private: bool,
    /// CIDR ranges a local TUN proxy answers with (treated as public).
    pub fake_ip_ranges: Vec<String>,
}

/// The `[network]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "restart"))]
pub struct NetworkConfig {
    /// Resolve and connect every outbound client (providers, MCP over HTTP,
    /// web tools, adapters, OTLP, downloads) over IPv4 only; loopback
    /// listeners are unaffected. Set it when `prometheus doctor network
    /// --online` reports a blackholed IPv6 route.
    pub force_ipv4: bool,
    /// Outbound HTTP(S) proxy URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,
    /// Extra PEM CA bundle trusted in addition to the platform store.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra_ca_file: Option<String>,
}

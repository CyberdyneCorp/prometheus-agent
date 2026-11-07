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
    /// The gateway daemon's health export (read from the launch profile).
    #[schemars(extend("x-apply" = "restart"))]
    pub gateway_health: GatewayHealthConfig,
}

/// The `[observability.gateway_health]` section (spec: messaging-gateway,
/// Gateway health export is opt-in and content-free): allowlisted gateway,
/// adapter, outbox and cron gauges and counters with bounded attributes,
/// posted as OTLP/HTTP JSON metrics; never message text, user ids, chat
/// ids or secrets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct GatewayHealthConfig {
    /// Export gateway health. Off by default: nothing is sent.
    pub enabled: bool,
    /// The OTLP/HTTP metrics endpoint (`https://`, or `http://` to a
    /// loopback collector), e.g. `http://127.0.0.1:4318/v1/metrics`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// Seconds between two exports.
    #[schemars(range(min = 1, max = 3600))]
    pub interval_secs: u64,
}

impl Default for GatewayHealthConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: None,
            interval_secs: 60,
        }
    }
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

/// The `[worktrees]` section (spec: git-worktrees).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "live"))]
pub struct WorktreesConfig {
    /// Fetch the default remote before creating a tree (a failed fetch
    /// falls back to the local tip).
    pub sync_remote: bool,
    /// Archive dirty or unpushed trees past their idle tier before
    /// removing them; `false` keeps such trees.
    pub archive_dirty: bool,
    /// Untracked files larger than this keep a tree from the sweep
    /// (`keep-large-untracked`) and are left out of archives.
    pub archive_max_file_bytes: u64,
    /// Total bytes of trees above which the oldest idle ones past their
    /// tier are archived and removed first (`0` = off).
    pub max_total_bytes: u64,
    /// Seconds an archive is kept before the sweep deletes it (`0` =
    /// forever).
    pub archive_retention_secs: u64,
}

impl Default for WorktreesConfig {
    fn default() -> Self {
        Self {
            sync_remote: true,
            archive_dirty: true,
            archive_max_file_bytes: 52_428_800,
            max_total_bytes: 0,
            archive_retention_secs: 2_592_000,
        }
    }
}

/// The `[vault]` section: the credential vault.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "next_session"))]
pub struct VaultConfig {
    /// Where a new vault's key is kept: `file` (`<profile>/vault/vault.key`)
    /// or `keychain` (the OS keychain; the file when none is reachable).
    /// `prometheus vault key migrate --to` moves an existing key.
    pub key_storage: VaultKeyStorage,
    /// The 1Password CLI as a read-only login source.
    pub onepassword: VaultSourceConfig,
    /// The Bitwarden CLI as a read-only login source.
    pub bitwarden: VaultSourceConfig,
}

/// Where the vault key is kept.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VaultKeyStorage {
    /// `<profile>/vault/vault.key` (0600).
    #[default]
    File,
    /// The OS keychain.
    Keychain,
}

/// A password-manager login source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct VaultSourceConfig {
    /// Offer its logins when its CLI is installed and signed in.
    pub enabled: bool,
}

impl Default for VaultSourceConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// The `[privacy]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "next_session"))]
pub struct PrivacyConfig {
    /// Replace phone numbers, other people's email addresses and messaging
    /// user ids (with a stable `user_<hash>`) in what reaches model
    /// providers, logs, trajectories and telemetry.
    pub redact_pii: bool,
    /// The profile owner's email addresses, which `redact_pii` keeps.
    pub owner_emails: Vec<String>,
}

/// The `[secrets]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "restart"))]
pub struct SecretsConfig {
    /// Where this profile's secrets are stored.
    pub backend: SecretsBackend,
    /// A helper command printing `NAME=VALUE` lines (POSIX only).
    pub command: SecretCommandConfig,
}

/// The `[secrets.command]` section: a helper run once per process start,
/// without a shell, whose stdout lines `NAME=VALUE` become secrets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SecretCommandConfig {
    /// Run the helper (off by default; unsupported on Windows).
    pub enabled: bool,
    /// The helper's program and arguments (no shell).
    pub argv: Vec<String>,
    /// Seconds before the helper's process group is killed.
    #[schemars(range(min = 1, max = 300))]
    pub timeout_secs: u64,
    /// Let helper values win over the secrets store and the environment.
    pub override_existing: bool,
}

impl Default for SecretCommandConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            argv: Vec::new(),
            timeout_secs: 3,
            override_existing: false,
        }
    }
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
    pub capabilities: Option<Vec<super::CapabilityText>>,
    /// OS sandbox.
    pub sandbox: SandboxConfig,
    /// Outbound network policy.
    pub egress: EgressConfig,
    /// Require human approval for agent writes to instruction files.
    pub protected_instruction_files: bool,
    /// Extra instruction-file basename globs (case-insensitive), e.g.
    /// `"*.mdc"`; writes to matches need human approval in every mode.
    pub protected_instruction_extra_patterns: Vec<String>,
    /// Supply-chain advisory ids whose startup warning is silenced
    /// (`prometheus doctor --ack <id>`); `doctor security` still shows them.
    pub acked_advisories: Vec<String>,
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
            acked_advisories: Vec::new(),
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
    /// Chromium's own sandbox (`browser.chromium_args` `--no-sandbox`...).
    ChromiumSandbox,
    /// Chromium's same-origin policy (`--disable-web-security`...).
    WebSecurity,
    /// Every feature.
    All,
}

impl DegradedFeature {
    /// The name used in `security.sandbox.allow_degraded`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Landlock => "landlock",
            Self::LandlockNet => "landlock_net",
            Self::Seccomp => "seccomp",
            Self::Seatbelt => "seatbelt",
            Self::Appcontainer => "appcontainer",
            Self::NoNewPrivileges => "no_new_privileges",
            Self::ReadOnlyRootfs => "read_only_rootfs",
            Self::CapDrop => "cap_drop",
            Self::ChromiumSandbox => "chromium_sandbox",
            Self::WebSecurity => "web_security",
            Self::All => "all",
        }
    }
}

/// `security.egress.mode`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EgressMode {
    /// Any public address; private and metadata ranges denied.
    #[default]
    Public,
    /// Only hosts named by `security.egress.allow` (or a session grant).
    Allowlist,
    /// No outbound connections.
    None,
}

impl EgressMode {
    /// Every mode, in documentation order.
    pub const ALL: [Self; 3] = [Self::Public, Self::Allowlist, Self::None];

    /// The configuration spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Allowlist => "allowlist",
            Self::None => "none",
        }
    }

    /// Parses a configuration spelling.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.as_str() == text)
    }
}

/// The `[security.egress]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct EgressConfig {
    /// Which hosts tools, sandboxed children, plugins and browsers may
    /// reach: any public address (`public`), only the `allow` patterns
    /// (`allowlist`) or nothing (`none`). A session can narrow or extend
    /// it (`chat --egress`, `/egress`, `egress.set`).
    pub mode: EgressMode,
    /// Host patterns `allowlist` admits: `host`, `host:port` or
    /// `*.suffix` (subdomains of `suffix`).
    pub allow: Vec<String>,
    /// Allow private destinations without a session grant.
    pub allow_private: bool,
    /// CIDR ranges a local TUN proxy answers with (treated as public).
    pub fake_ip_ranges: Vec<String>,
    /// Hosts refused in every mode (`*` globs, `host` or `host:port`, e.g.
    /// `*.casino.example`); a session can add to them, never remove.
    pub deny: Vec<String>,
    /// Absolute paths of shared blocklist files: one pattern per line, `#`
    /// comments; re-read when a file changes.
    pub deny_files: Vec<String>,
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

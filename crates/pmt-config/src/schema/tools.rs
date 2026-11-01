//! Tool, approval, terminal, browser and web sections (specs: tool-system,
//! execution-environments, web-and-media-tools, workspace-architecture).

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The `[tools]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "next_session"))]
pub struct ToolsConfig {
    /// Toolsets exposed to sessions.
    pub enabled_toolsets: Vec<String>,
    /// Toolsets removed last, even when a composite lists them.
    pub disabled_toolsets: Vec<String>,
    /// Timeout of a tool call that declares none.
    pub default_timeout_secs: u64,
    /// Wait between SIGTERM and SIGKILL when cancelling tool processes.
    pub kill_grace_secs: u64,
    /// Tool result size budgets.
    pub result_budget: ResultBudget,
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            enabled_toolsets: vec!["core".to_owned()],
            disabled_toolsets: Vec::new(),
            default_timeout_secs: 300,
            kill_grace_secs: 5,
            result_budget: ResultBudget::default(),
        }
    }
}

/// The `[tools.result_budget]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ResultBudget {
    /// Characters kept inline for tools without a declared cap.
    pub default_chars: u64,
    /// Characters kept inline for MCP tools.
    pub mcp_chars: u64,
    /// Per-tool overrides: tool name to characters.
    pub overrides: BTreeMap<String, u64>,
}

impl Default for ResultBudget {
    fn default() -> Self {
        Self {
            default_chars: 100_000,
            mcp_chars: 50_000,
            overrides: BTreeMap::new(),
        }
    }
}

/// The `[toolsets]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "next_session"))]
pub struct ToolsetsConfig {
    /// Default toolsets per surface (otherwise `core`).
    pub surfaces: BTreeMap<String, Vec<String>>,
}

/// The `[approvals]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "live"))]
pub struct ApprovalsConfig {
    /// Approval mode of interactive sessions.
    pub mode: ApprovalMode,
    /// Decision for approvals in cron jobs.
    pub cron_mode: UnattendedApproval,
    /// Decision for approvals in single-query (`-q`) runs.
    pub single_query_mode: UnattendedApproval,
    /// Decision for approvals in other unattended sessions.
    pub unattended_mode: UnattendedApproval,
    /// Unanswered approvals are denied after this long.
    pub timeout_secs: u64,
    /// Command patterns that are always blocked.
    pub deny: Vec<String>,
}

impl Default for ApprovalsConfig {
    fn default() -> Self {
        Self {
            mode: ApprovalMode::Manual,
            cron_mode: UnattendedApproval::Deny,
            single_query_mode: UnattendedApproval::Deny,
            unattended_mode: UnattendedApproval::Deny,
            timeout_secs: 300,
            deny: Vec::new(),
        }
    }
}

/// Interactive approval modes. In TOML they are strings: `mode = "off"`;
/// the YAML habit `mode = off` does not parse and `mode = false` is not a
/// mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMode {
    /// Ask for every dangerous call.
    Manual,
    /// An auxiliary model pre-screens; uncertain calls are asked.
    Smart,
    /// No prompts (deny rules and hardline blocks still apply).
    Off,
}

/// Decisions for approvals nobody can answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum UnattendedApproval {
    /// Deny.
    Deny,
    /// Approve.
    Approve,
}

/// The `[tool_output]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "live"))]
pub struct ToolOutputConfig {
    /// Terminal output characters kept (40% head, 60% tail).
    pub max_chars: u64,
}

impl Default for ToolOutputConfig {
    fn default() -> Self {
        Self { max_chars: 50_000 }
    }
}

/// The `[files]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "live"))]
pub struct FilesConfig {
    /// Formatted characters `read_file` returns per call.
    pub read_max_chars: u64,
}

impl Default for FilesConfig {
    fn default() -> Self {
        Self {
            read_max_chars: 100_000,
        }
    }
}

/// The `[terminal]` section.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "next_session"))]
pub struct TerminalConfig {
    /// Backend: `local`, `container`, `ssh` or a plugin backend.
    pub backend: String,
    /// Start directory for `local`; unset is the launch directory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Default foreground command timeout.
    pub timeout_secs: u64,
    /// Longer foreground calls become tracked background processes.
    pub max_foreground_secs: u64,
    /// Idle time before a cached environment is torn down.
    pub idle_lifetime_secs: u64,
    /// Keep one shell per session.
    pub persistent_shell: bool,
    /// Source `~/.bashrc` once when the shell is bash.
    pub auto_source_bashrc: bool,
    /// Extra files sourced at shell start (`~` expanded).
    pub shell_init_files: Vec<String>,
    /// Bound on the whole shell init step.
    pub shell_init_timeout_secs: u64,
    /// Non-secret launch-environment variables passed to children.
    pub env_passthrough: Vec<String>,
    /// Backend connection failures: degraded results or errors.
    pub degraded_mode: DegradedMode,
    /// Windows shell.
    pub windows_shell: WindowsShell,
    /// Git Bash path tried first by `windows_shell = "auto"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub windows_bash_path: Option<String>,
    /// Container backend.
    pub container: ContainerConfig,
    /// SSH backend.
    pub ssh: SshConfig,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            backend: "local".to_owned(),
            cwd: None,
            timeout_secs: 180,
            max_foreground_secs: 600,
            idle_lifetime_secs: 300,
            persistent_shell: true,
            auto_source_bashrc: true,
            shell_init_files: Vec::new(),
            shell_init_timeout_secs: 10,
            env_passthrough: Vec::new(),
            degraded_mode: DegradedMode::Degrade,
            windows_shell: WindowsShell::Auto,
            windows_bash_path: None,
            container: ContainerConfig::default(),
            ssh: SshConfig::default(),
        }
    }
}

/// Backend failure handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DegradedMode {
    /// A `degraded` result with a retry hint.
    Degrade,
    /// An `error` result.
    Fail,
}

/// Windows shells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum WindowsShell {
    /// Git Bash, else PowerShell.
    Auto,
    /// Git Bash.
    GitBash,
    /// PowerShell 7.
    Pwsh,
    /// Windows PowerShell.
    Powershell,
}

/// The `[terminal.container]` section.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ContainerConfig {
    /// CPU limit.
    #[schemars(range(min = 0.0))]
    pub cpus: f64,
    /// Memory limit.
    pub memory_bytes: u64,
    /// Disk limit.
    pub disk_bytes: u64,
    /// Keep the container between sessions.
    pub persistent: bool,
    /// `/dev/shm` size in the runtime's syntax.
    pub shm_size: String,
    /// Process limit.
    pub pids_limit: u32,
    /// Literal environment set in the container.
    pub env: BTreeMap<String, String>,
}

impl Default for ContainerConfig {
    fn default() -> Self {
        Self {
            cpus: 1.0,
            memory_bytes: 5_368_709_120,
            disk_bytes: 53_687_091_200,
            persistent: true,
            shm_size: "1g".to_owned(),
            pids_limit: 2048,
            env: BTreeMap::new(),
        }
    }
}

/// The `[terminal.ssh]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SshConfig {
    /// Host name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// User name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Port.
    #[schemars(range(min = 1, max = 65_535))]
    pub port: u32,
}

impl Default for SshConfig {
    fn default() -> Self {
        Self {
            host: None,
            user: None,
            port: 22,
        }
    }
}

/// The `[browser]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "next_session"))]
pub struct BrowserConfig {
    /// Browser provider: `local` or a cloud browser plugin.
    pub provider: String,
    /// Explicit CDP endpoint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cdp_url: Option<String>,
    /// JavaScript dialog handling.
    pub dialog_policy: DialogPolicy,
    /// Unanswered dialogs are dismissed after this long.
    pub dialog_timeout_secs: u64,
    /// Idle browser sessions are closed after this long.
    #[schemars(extend("x-apply" = "live"))]
    pub inactivity_timeout_secs: u64,
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            provider: "local".to_owned(),
            cdp_url: None,
            dialog_policy: DialogPolicy::MustRespond,
            dialog_timeout_secs: 300,
            inactivity_timeout_secs: 120,
        }
    }
}

/// Dialog policies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DialogPolicy {
    /// The model must answer.
    MustRespond,
    /// Dismiss automatically.
    AutoDismiss,
    /// Accept automatically.
    AutoAccept,
}

/// The `[web]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "next_session"))]
pub struct WebConfig {
    /// Shared backend for web search and extract (`auto` picks the first
    /// with credentials).
    pub backend: String,
    /// Search backend; unset uses `backend`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search_backend: Option<String>,
    /// Extract backend; unset uses `backend`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extract_backend: Option<String>,
    /// Characters per extracted page.
    #[schemars(range(min = 2000, max = 500_000))]
    pub extract_char_limit: u64,
    /// Bound on one extract dispatch.
    pub extract_timeout_secs: u64,
    /// Keyless web search.
    pub keyless: KeylessConfig,
    /// The `firecrawl` backend (search and extract).
    pub firecrawl: FirecrawlConfig,
    /// The `searxng` backend (search only).
    pub searxng: SearxngConfig,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            backend: "auto".to_owned(),
            search_backend: None,
            extract_backend: None,
            extract_char_limit: 15_000,
            extract_timeout_secs: 120,
            keyless: KeylessConfig::default(),
            firecrawl: FirecrawlConfig::default(),
            searxng: SearxngConfig::default(),
        }
    }
}

/// The `[web.firecrawl]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct FirecrawlConfig {
    /// Self-hosted Firecrawl base URL; unset is the hosted API
    /// (`https://api.firecrawl.dev`, secret `FIRECRAWL_API_KEY`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
}

/// The `[web.searxng]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SearxngConfig {
    /// Base URL of the SearXNG instance (its JSON output must be enabled);
    /// `searxng` is unavailable until it is set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// The `[web.keyless]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct KeylessConfig {
    /// Allow keyless web search as the last `auto` choice.
    pub enabled: bool,
}

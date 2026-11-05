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
    /// Progressive disclosure of large tool surfaces.
    pub tool_search: ToolSearchConfig,
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            enabled_toolsets: vec!["core".to_owned()],
            disabled_toolsets: Vec::new(),
            default_timeout_secs: 300,
            kill_grace_secs: 5,
            result_budget: ResultBudget::default(),
            tool_search: ToolSearchConfig::default(),
        }
    }
}

/// The `[tools.tool_search]` section (spec: tool-system, Tool search
/// progressive disclosure; Tool search listing, ranking and limits).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ToolSearchConfig {
    /// `auto` defers MCP and plugin tools behind `tool_search`,
    /// `tool_describe` and `tool_call` when their schemas outgrow the
    /// thresholds below; `on` always does, `off` never.
    pub mode: ToolSearchMode,
    /// Percent of the main model's context window the deferrable tools'
    /// schemas may take before `auto` defers them.
    #[schemars(range(min = 0, max = 100))]
    pub threshold_pct: u32,
    /// Token budget (4 characters each) of the deferrable schemas, and of
    /// the deferred-tool manifest in the `tool_search` description.
    #[schemars(range(min = 200, max = 60_000))]
    pub listing_max_tokens: u32,
    /// Tools deferred besides MCP and plugin tools while tool search is
    /// active; unset defers `computer_use`, `session_search`,
    /// `image_generate`, `todo`, `process`, `cronjob` and `browser_vault`.
    pub defer: Option<Vec<String>>,
    /// Matches per query when `tool_search` names no `limit`.
    #[schemars(range(min = 1, max = 50))]
    pub search_default_limit: u32,
    /// The most matches per query `tool_search` returns.
    #[schemars(range(min = 1, max = 50))]
    pub max_search_limit: u32,
    /// Whether the `tool_search` description lists the deferred tools.
    pub listing: ToolListing,
}

impl Default for ToolSearchConfig {
    fn default() -> Self {
        Self {
            mode: ToolSearchMode::Auto,
            threshold_pct: 5,
            listing_max_tokens: 4_000,
            defer: None,
            search_default_limit: 5,
            max_search_limit: 25,
            listing: ToolListing::Auto,
        }
    }
}

/// `tools.tool_search.mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolSearchMode {
    /// Defer when the deferrable schemas outgrow the thresholds.
    Auto,
    /// Always defer.
    On,
    /// Never defer.
    Off,
}

/// `tools.tool_search.listing`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolListing {
    /// List the deferred tools within the listing budget.
    Auto,
    /// The same as `auto`.
    On,
    /// No listing.
    Off,
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
    /// Starting toolsets per session surface, replacing the surface's
    /// default (spec: tool-system, Per-surface default toolsets).
    pub surfaces: SurfaceToolsets,
}

/// `[toolsets.surfaces]`: one optional toolset list per session surface.
/// An unset surface starts from `tools.enabled_toolsets`, where `core`
/// stands for the surface's built-in default.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SurfaceToolsets {
    /// Line-mode chat and one-shot runs (default `core`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cli: Option<Vec<String>>,
    /// The terminal UI (default `core`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tui: Option<Vec<String>>,
    /// The desktop app (default `core`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub desktop: Option<Vec<String>>,
    /// Scheduled jobs without per-job toolsets (default `core`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cron: Option<Vec<String>>,
    /// The OpenAI-compatible API server (default `api_server`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api: Option<Vec<String>>,
    /// ACP editor sessions (default `acp`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acp: Option<Vec<String>>,
    /// Unattended ACP sessions (default `acp_unattended`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acp_unattended: Option<Vec<String>>,
    /// Messaging sessions per platform (`gateway.<platform>`, default
    /// `platform-<platform>`; `gateway.webhook` defaults to
    /// `webhook-safe`).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub gateway: BTreeMap<String, Vec<String>>,
}

impl SurfaceToolsets {
    /// The surfaces with a key of their own (besides `gateway.<platform>`).
    pub const KEYS: &[&str] = &[
        "cli",
        "tui",
        "desktop",
        "cron",
        "api",
        "acp",
        "acp_unattended",
    ];

    /// The list saved for surface `key` (`cli`, `gateway.telegram`...).
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Vec<String>> {
        if let Some(platform) = key.strip_prefix("gateway.") {
            return self.gateway.get(platform);
        }
        match key {
            "cli" => self.cli.as_ref(),
            "tui" => self.tui.as_ref(),
            "desktop" => self.desktop.as_ref(),
            "cron" => self.cron.as_ref(),
            "api" => self.api.as_ref(),
            "acp" => self.acp.as_ref(),
            "acp_unattended" => self.acp_unattended.as_ref(),
            _ => None,
        }
    }

    /// Every saved list with its surface key, in key order.
    #[must_use]
    pub fn entries(&self) -> Vec<(String, &Vec<String>)> {
        let mut out: Vec<(String, &Vec<String>)> = Self::KEYS
            .iter()
            .filter_map(|key| self.get(key).map(|list| ((*key).to_owned(), list)))
            .collect();
        out.extend(
            self.gateway
                .iter()
                .map(|(platform, list)| (format!("gateway.{platform}"), list)),
        );
        out
    }
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
    /// In `smart` mode, after this many consecutive denials by the
    /// approval guardian the denial tells the model to stop trying
    /// variations (`0` disables); a human approval resets the count.
    pub denial_breaker_threshold: u32,
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
            denial_breaker_threshold: 3,
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
    /// File snapshots, undo and rollback checkpoints.
    pub snapshot: SnapshotConfig,
}

impl Default for FilesConfig {
    fn default() -> Self {
        Self {
            read_max_chars: 100_000,
            snapshot: SnapshotConfig::default(),
        }
    }
}

/// The `[files.snapshot]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SnapshotConfig {
    /// Record file-tool changes (`/undo`) and checkpoints (`/rollback`);
    /// `chat --checkpoints` / `--no-checkpoints` override it per run.
    pub enabled: bool,
    /// Also checkpoint before destructive terminal commands (`rm`, `mv`,
    /// `sed -i`, `>`, `git reset`...).
    pub terminal: SnapshotTerminal,
    /// Larger files are listed as skipped instead of stored.
    pub max_file_bytes: u64,
    /// Checkpoints kept per workspace.
    pub max_checkpoints: u64,
    /// Snapshots unused this long are pruned.
    pub retention_secs: u64,
    /// The profile's snapshot store is pruned, oldest first, above this.
    pub max_bytes: u64,
    /// Least time between automatic prunes.
    pub prune_interval_secs: u64,
}

impl Default for SnapshotConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            terminal: SnapshotTerminal::Off,
            max_file_bytes: 10_485_760,
            max_checkpoints: 20,
            retention_secs: 604_800,
            max_bytes: 1_073_741_824,
            prune_interval_secs: 86_400,
        }
    }
}

/// `files.snapshot.terminal`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotTerminal {
    /// Terminal commands never checkpoint.
    #[default]
    Off,
    /// Checkpoint the command's project before a destructive command.
    Destructive,
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
    /// Copy files changed in a remote workspace back to the host workspace
    /// at session end and on `/sync`; unset is `true` for `ssh` and `false`
    /// for every other backend.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync_back: Option<bool>,
    /// Total bytes one sync-back copies; larger files are skipped and
    /// listed.
    pub sync_back_max_bytes: u64,
    /// The first-party `modal` backend plugin.
    pub modal: ModalConfig,
    /// The first-party `singularity` backend plugin.
    pub singularity: SingularityConfig,
    /// Two `process` `poll`/`wait` calls in a row that find a running
    /// process silent switch on its completion notification (where the
    /// session can receive one).
    pub process_wait_auto_notify: bool,
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
            sync_back: None,
            sync_back_max_bytes: 2_147_483_648,
            modal: ModalConfig::default(),
            singularity: SingularityConfig::default(),
            process_wait_auto_notify: true,
        }
    }
}

impl TerminalConfig {
    /// Whether sync-back is on for the selected backend.
    #[must_use]
    pub fn sync_back_enabled(&self) -> bool {
        self.sync_back.unwrap_or(self.backend == "ssh")
    }
}

/// The `[terminal.modal]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ModalConfig {
    /// How the plugin authenticates.
    pub mode: ModalMode,
}

/// `terminal.modal.mode`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModalMode {
    /// `direct` when Modal secrets exist, else `managed` when a
    /// subscription is signed in, else unavailable.
    #[default]
    Auto,
    /// The user's `MODAL_TOKEN_ID`/`MODAL_TOKEN_SECRET` secrets.
    Direct,
    /// A subscription gateway credential.
    Managed,
}

/// The `[terminal.singularity]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SingularityConfig {
    /// A SIF path or a `docker://` reference.
    pub image: String,
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
#[allow(clippy::struct_excessive_bools)] // independent switches
pub struct ContainerConfig {
    /// CPU limit.
    #[schemars(range(min = 0.0))]
    pub cpus: f64,
    /// Memory limit.
    pub memory_bytes: u64,
    /// Disk limit.
    pub disk_bytes: u64,
    /// One container per profile and workspace, reused by later sessions
    /// and stopped (never removed) when idle; `false` gives each session
    /// its own container.
    pub persistent: bool,
    /// `/dev/shm` size in the runtime's syntax.
    pub shm_size: String,
    /// Process limit.
    pub pids_limit: u32,
    /// Literal environment set in the container.
    pub env: BTreeMap<String, String>,
    /// Runtime: `auto` (rootless Podman, else Docker), `podman`, `docker`
    /// or the path of either executable.
    pub runtime: String,
    /// Image; empty is the release's `prometheus-sandbox` image.
    pub image: String,
    /// Extra mounts, `host:container[:ro]`.
    pub volumes: Vec<String>,
    /// Mount the session workspace at `/workspace`.
    pub mount_workspace: bool,
    /// Give the container a network; `false` runs it with none.
    pub network: bool,
    /// Run as the host user's uid and gid.
    pub run_as_host_user: bool,
    /// Sessions of the profile with the same key share one container.
    pub shared_key: String,
    /// Extra `create` arguments; anything weakening hardening is refused.
    pub extra_args: Vec<String>,
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
            runtime: "auto".to_owned(),
            image: String::new(),
            volumes: Vec::new(),
            mount_workspace: true,
            network: true,
            run_as_host_user: false,
            shared_key: String::new(),
            extra_args: Vec::new(),
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
    /// An `ssh_config` file passed with `-F` (identity, known hosts, jump
    /// hosts); unset uses the user's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_file: Option<String>,
    /// The remote workspace synced back to the host.
    pub workspace: String,
}

impl Default for SshConfig {
    fn default() -> Self {
        Self {
            host: None,
            user: None,
            port: 22,
            config_file: None,
            workspace: "~/prometheus-workspace".to_owned(),
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

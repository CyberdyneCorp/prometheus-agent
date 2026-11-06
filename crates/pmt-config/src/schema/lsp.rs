//! The `[lsp]` section (spec: lsp-diagnostics): which language servers
//! run after file edits, where they are trusted, how long an edit waits
//! for their verdict and how missing servers are installed.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The ids of the built-in language servers (`lsp.servers.<id>` overrides
/// one of them; any other key declares a custom server).
pub const LSP_SERVER_IDS: &[&str] = &[
    "rust-analyzer",
    "pyright",
    "typescript-language-server",
    "gopls",
    "clangd",
    "bash-language-server",
    "yaml-language-server",
    "dockerfile-language-server",
    "vue-language-server",
    "svelte-language-server",
    "astro-language-server",
    "intelephense",
    "lua-language-server",
    "terraform-ls",
    "jdtls",
    "kotlin-language-server",
    "elixir-ls",
    "zls",
    "clojure-lsp",
    "haskell-language-server",
    "ocaml-lsp",
    "prisma-language-server",
    "cmake-language-server",
];

/// The `[lsp]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "next_session"))]
pub struct LspConfig {
    /// Run language servers after `write_file` and `patch`; `false` starts
    /// no server, watcher or background task.
    pub enabled: bool,
    /// Git roots where no server runs (globs, `~` expanded; a bare path
    /// also matches everything below it).
    pub exclude_roots: Vec<String>,
    /// Workspaces trusted beyond the one the session was pointed at
    /// (globs, `~` expanded; a bare path also matches everything below
    /// it). Servers that execute project code start only in trusted
    /// workspaces.
    pub trusted_workspaces: Vec<String>,
    /// `document` checks the edited file; `full` also asks for workspace
    /// diagnostics.
    pub wait_mode: LspWaitMode,
    /// Longest wait for a verdict before the turn's final answer is
    /// accepted, and the bound of one check.
    #[schemars(range(min = 1, max = 600_000))]
    pub wait_timeout_ms: u64,
    /// Bound of the first check against a server that is not running yet
    /// (`0` = `wait_timeout_ms`).
    #[schemars(range(max = 600_000))]
    pub warmup_timeout_ms: u64,
    /// After a server fails to start or times out, skip it for its
    /// workspace this long (`0` = until `prometheus lsp restart` or the
    /// process exits).
    pub broken_retry_secs: u64,
    /// Shut down a server idle this long; it restarts on demand (`0` =
    /// never).
    pub idle_timeout_secs: u64,
    /// `auto` installs missing `npm`, `go` and `pip` servers on first use;
    /// `manual` uses only binaries on `PATH`.
    pub install_strategy: LspInstallStrategy,
    /// The package manager of `npm`-method installs; one that is not
    /// installed skips the install (never another manager).
    pub package_manager: LspPackageManager,
    /// Overrides of built-in servers by id, and custom servers under any
    /// other key.
    pub servers: BTreeMap<String, LspServerConfig>,
}

impl Default for LspConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            exclude_roots: Vec::new(),
            trusted_workspaces: Vec::new(),
            wait_mode: LspWaitMode::Document,
            wait_timeout_ms: 5_000,
            warmup_timeout_ms: 0,
            broken_retry_secs: 0,
            idle_timeout_secs: 600,
            install_strategy: LspInstallStrategy::Auto,
            package_manager: LspPackageManager::Npm,
            servers: BTreeMap::new(),
        }
    }
}

impl LspConfig {
    /// The bound of the first check against a server not running yet.
    #[must_use]
    pub fn warmup_ms(&self) -> u64 {
        if self.warmup_timeout_ms == 0 {
            self.wait_timeout_ms
        } else {
            self.warmup_timeout_ms
        }
    }
}

/// What a check asks the server for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LspWaitMode {
    /// Diagnostics of the edited file.
    Document,
    /// The edited file's and the workspace's.
    Full,
}

/// How missing servers are obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LspInstallStrategy {
    /// Install on first use into `<PROMETHEUS_HOME>/runtimes/lsp/`.
    Auto,
    /// Only binaries on `PATH`.
    Manual,
}

/// The package manager of `npm`-method installs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LspPackageManager {
    /// npm (the managed Node's own when installed).
    Npm,
    /// pnpm.
    Pnpm,
    /// Yarn.
    Yarn,
}

impl LspPackageManager {
    /// `npm`, `pnpm` or `yarn`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Npm => "npm",
            Self::Pnpm => "pnpm",
            Self::Yarn => "yarn",
        }
    }
}

/// One `[lsp.servers.<id>]` entry: an override of a built-in server, or
/// a custom server (`command` and `extensions` required).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct LspServerConfig {
    /// Never start this server.
    pub disabled: bool,
    /// The server's argv; bypasses installation.
    pub command: Option<Vec<String>>,
    /// Extra environment of the server (non-secret values).
    pub env: BTreeMap<String, String>,
    /// Merged into the server's initialization options.
    #[schemars(with = "BTreeMap<String, serde_json::Value>")]
    pub initialization_options: serde_json::Map<String, serde_json::Value>,
    /// Custom servers: the file extensions it checks (`.ml2`).
    pub extensions: Vec<String>,
    /// Custom servers: files whose directory is the server's root.
    pub root_markers: Vec<String>,
    /// Custom servers: the LSP language id (default: the extension).
    pub language_id: Option<String>,
    /// Custom servers: what it is, for `prometheus lsp list`.
    pub description: Option<String>,
}

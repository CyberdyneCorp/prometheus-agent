//! `lsp.status` (spec: lsp-diagnostics, LSP CLI): the running language
//! servers of a profile, as `prometheus lsp status` shows them.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::Method;

/// `lsp.status` params.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LspStatusParams {
    /// The profile (the connection's only one when omitted).
    #[serde(default)]
    pub profile: Option<String>,
}

/// One running language server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LspServerStatus {
    /// The server id (`pyright`, `rust-analyzer`...).
    pub id: String,
    /// Its workspace root.
    pub workspace: String,
    /// Its process id.
    pub pid: u32,
    /// Seconds since it started.
    pub uptime_secs: u64,
    /// `ready`.
    pub state: String,
    /// It executes project code, so it starts only in trusted workspaces.
    pub trusted_only: bool,
    /// The session that started it.
    pub session_id: String,
}

/// `lsp.status` result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LspStatus {
    /// Every running server of the profile, by id and workspace.
    pub servers: Vec<LspServerStatus>,
}

method!(/// `lsp.status`: the profile's running language servers.
    LspStatusCall = "lsp.status" (LspStatusParams) -> LspStatus);

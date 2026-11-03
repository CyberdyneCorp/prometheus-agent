//! The `[mcp]` and `[mcp_server]` sections (spec: plugin-system, MCP client
//! servers; MCP server mode; tool-system, MCP tool naming and filtering; MCP
//! runtime safety).

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The `[mcp]` section: the external MCP servers the profile's sessions
/// connect to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "next_session"))]
pub struct McpConfig {
    /// Servers by name; their tools register as `mcp__<name>__<tool>` in
    /// toolset `mcp-<name>`.
    pub servers: BTreeMap<String, McpServerConfig>,
    /// Servers connected at once when a session needs them.
    #[schemars(range(min = 1, max = 64))]
    pub discovery_concurrency: u32,
    /// Bound on one tool call of a server that sets no `timeout_secs`.
    #[schemars(range(min = 1, max = 86_400))]
    pub call_timeout_secs: u64,
    /// Bound on starting and initializing a server that sets no
    /// `connect_timeout_secs`.
    #[schemars(range(min = 1, max = 3_600))]
    pub connect_timeout_secs: u64,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            servers: BTreeMap::new(),
            discovery_concurrency: 4,
            call_timeout_secs: 300,
            connect_timeout_secs: 60,
        }
    }
}

/// One `[mcp.servers.<name>]` entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct McpServerConfig {
    /// `false` skips the server without connecting.
    pub enabled: bool,
    /// How the server is reached.
    pub transport: McpTransport,
    /// The program of a `stdio` server (a bare name is looked up on
    /// `PATH`).
    pub command: String,
    /// Its arguments.
    pub args: Vec<String>,
    /// Environment variables of a `stdio` server, literal values. A
    /// `stdio` server gets only these, `env_secrets` and `PATH`, `HOME`,
    /// `LANG`, `SYSTEMROOT` and `TMPDIR`.
    pub env: BTreeMap<String, String>,
    /// Secrets-store names passed to a `stdio` server as environment
    /// variables of the same name.
    pub env_secrets: Vec<String>,
    /// The endpoint of an `http` (streamable HTTP) server.
    pub url: String,
    /// Secrets-store name of the bearer token sent to an `http` server.
    pub auth_secret: String,
    /// `untrusted` servers' tools need approval unless they declare
    /// `readOnlyHint`; `full` trusts them like built-in tools.
    pub trust: McpTrust,
    /// What a `stdio` server's sandbox lets it reach, like a plugin grant.
    pub capabilities: McpCapabilities,
    /// Which of the server's tools register.
    pub tools: McpToolFilter,
    /// Overrides `mcp.call_timeout_secs` for this server.
    #[schemars(range(min = 1, max = 86_400))]
    pub timeout_secs: Option<u64>,
    /// Overrides `mcp.connect_timeout_secs` for this server.
    #[schemars(range(min = 1, max = 3_600))]
    pub connect_timeout_secs: Option<u64>,
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            transport: McpTransport::Stdio,
            command: String::new(),
            args: Vec::new(),
            env: BTreeMap::new(),
            env_secrets: Vec::new(),
            url: String::new(),
            auth_secret: String::new(),
            trust: McpTrust::Untrusted,
            capabilities: McpCapabilities::default(),
            tools: McpToolFilter::default(),
            timeout_secs: None,
            connect_timeout_secs: None,
        }
    }
}

/// `mcp.servers.<name>.transport`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum McpTransport {
    /// A child process speaking JSON-RPC on its stdin and stdout.
    Stdio,
    /// A streamable HTTP endpoint.
    Http,
}

impl McpTransport {
    /// The configured spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stdio => "stdio",
            Self::Http => "http",
        }
    }
}

/// `mcp.servers.<name>.trust`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum McpTrust {
    /// Trusted like a built-in tool.
    Full,
    /// Every tool not declaring `readOnlyHint` needs approval.
    Untrusted,
}

/// `[mcp.servers.<name>.capabilities]`: the sandbox of a `stdio` server,
/// in the words of a plugin manifest.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct McpCapabilities {
    /// Hosts it may connect to (`api.github.com`, `*.example.com`).
    pub network: Vec<String>,
    /// Paths it may read: absolute, or under `$workspace`.
    pub fs_read: Vec<String>,
    /// Paths it may write: absolute, or under `$workspace`.
    pub fs_write: Vec<String>,
}

/// `[mcp.servers.<name>.tools]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct McpToolFilter {
    /// Whitelist of the server's tool names (exact or case-sensitive
    /// globs); wins over `exclude`, and an empty list registers nothing.
    pub include: Option<Vec<String>>,
    /// Tool names (exact or globs) never registered.
    pub exclude: Vec<String>,
}

/// The `[mcp_server]` section: what `prometheus mcp serve` offers MCP
/// clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "restart"))]
pub struct McpServerModeConfig {
    /// Toolsets exposed as MCP tools; `skills` exposes only the read-only
    /// skill tools.
    pub expose_toolsets: Vec<String>,
    /// Let `--http` bind an address that is not loopback.
    pub allow_remote: bool,
}

impl Default for McpServerModeConfig {
    fn default() -> Self {
        Self {
            expose_toolsets: vec!["skills".to_owned()],
            allow_remote: false,
        }
    }
}

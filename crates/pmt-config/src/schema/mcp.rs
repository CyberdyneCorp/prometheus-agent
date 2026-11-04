//! The `[mcp]` and `[mcp_server]` sections (spec: plugin-system, MCP client
//! servers; MCP server mode; MCP OAuth for HTTP servers; MCP mTLS and
//! identity header; MCP config watch, recycling and discovery wait; MCP
//! sampling; MCP elicitation; MCP tool costs and spend limits; tool-system,
//! MCP tool naming and filtering; MCP runtime safety).

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The `[mcp]` section: the external MCP servers the profile's sessions
/// connect to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
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
    /// The daemon applies `mcp.servers` edits within a minute (new
    /// sessions only).
    pub auto_reload_on_config_change: bool,
    /// How long an interactive session start waits for servers still
    /// connecting; later ones join later sessions.
    #[schemars(range(max = 600_000))]
    pub discovery_wait_ms: u64,
    /// The same wait for one-shot `-q` runs.
    #[schemars(range(max = 600_000))]
    pub single_query_discovery_wait_ms: u64,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            servers: BTreeMap::new(),
            discovery_concurrency: 4,
            call_timeout_secs: 300,
            connect_timeout_secs: 60,
            auto_reload_on_config_change: true,
            discovery_wait_ms: 1_500,
            single_query_discovery_wait_ms: 15_000,
        }
    }
}

/// One `[mcp.servers.<name>]` entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
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
    /// How an `http` server authenticates: `header` (the `auth_secret`
    /// bearer token) or `oauth` (OAuth 2.1 with PKCE, `prometheus mcp
    /// login`).
    pub auth: Option<McpAuth>,
    /// The OAuth client of an `auth = "oauth"` server.
    pub oauth: McpOAuthConfig,
    /// Headers sent with every request to an `http` server (`${NAME}`
    /// placeholders come from the secrets store).
    pub headers: BTreeMap<String, String>,
    /// A PEM file holding the client certificate (and its key unless
    /// `client_key` is set) presented to an `https` server.
    pub client_cert: String,
    /// The PEM file of the client certificate's private key.
    pub client_key: String,
    /// Secrets-store name of the password of an encrypted `client_key`.
    pub client_key_password_secret: String,
    /// `{ name, value_from = "static" | "profile", value }`: a header sent
    /// with every request naming the caller (the literal `value` or the
    /// profile name); never overrides a same-named `headers` entry. An
    /// invalid block, or one on a `stdio` server, is warned and ignored.
    #[schemars(with = "Option<serde_json::Map<String, serde_json::Value>>")]
    pub identity_header: Option<serde_json::Value>,
    /// Register the tools from the schema cache and start the server on
    /// its first call.
    pub lazy: bool,
    /// Stop a `stdio` server after this many idle seconds (0: never); it
    /// restarts on its next call.
    pub idle_timeout_secs: u64,
    /// Stop a `stdio` server this many seconds after it started (0:
    /// never); it restarts on its next call.
    pub max_lifetime_secs: u64,
    /// `sampling/createMessage` answers.
    pub sampling: McpSamplingConfig,
    /// `elicitation/create` answers.
    pub elicitation: McpElicitationConfig,
    /// What the server's tools cost: a number, or `{ credits = [min, max],
    /// note = "..." }`, per tool name. A malformed entry skips the server.
    #[schemars(with = "BTreeMap<String, serde_json::Value>")]
    pub tool_costs: BTreeMap<String, serde_json::Value>,
    /// The most one conversation may spend on this server's tools before
    /// each further costed call asks (0: no limit).
    #[schemars(range(min = 0.0))]
    pub spend_limit: f64,
    /// The unit of `tool_costs` and `spend_limit`.
    pub cost_unit: String,
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
            auth: None,
            oauth: McpOAuthConfig::default(),
            headers: BTreeMap::new(),
            client_cert: String::new(),
            client_key: String::new(),
            client_key_password_secret: String::new(),
            identity_header: None,
            lazy: false,
            idle_timeout_secs: 0,
            max_lifetime_secs: 0,
            sampling: McpSamplingConfig::default(),
            elicitation: McpElicitationConfig::default(),
            tool_costs: BTreeMap::new(),
            spend_limit: 0.0,
            cost_unit: "credits".to_owned(),
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

/// `mcp.servers.<name>.auth`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum McpAuth {
    /// The `auth_secret` bearer token.
    Header,
    /// OAuth 2.1 with PKCE.
    Oauth,
}

/// `[mcp.servers.<name>.oauth]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct McpOAuthConfig {
    /// A client registered with the authorization server beforehand (for
    /// servers without dynamic client registration).
    pub client_id: String,
    /// Secrets-store name of that client's secret.
    pub client_secret_name: String,
    /// The client name sent at dynamic registration.
    pub client_name: String,
    /// Requested scopes (default: what the server advertises).
    pub scopes: Vec<String>,
    /// The redirect host: `127.0.0.1` or `localhost`.
    pub redirect_host: McpRedirectHost,
    /// A fixed callback port (0: any free port).
    pub redirect_port: u16,
    /// A full redirect URI registered with the client (overrides host and
    /// port).
    pub redirect_uri: String,
    /// `browser` (authorization code) or `device` (device code).
    pub flow: McpOAuthFlow,
    /// How long a login waits for the person.
    #[schemars(range(min = 10, max = 3_600))]
    pub timeout_secs: u64,
}

impl Default for McpOAuthConfig {
    fn default() -> Self {
        Self {
            client_id: String::new(),
            client_secret_name: String::new(),
            client_name: String::new(),
            scopes: Vec::new(),
            redirect_host: McpRedirectHost::Loopback,
            redirect_port: 0,
            redirect_uri: String::new(),
            flow: McpOAuthFlow::Browser,
            timeout_secs: 300,
        }
    }
}

/// `mcp.servers.<name>.oauth.redirect_host`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum McpRedirectHost {
    /// `127.0.0.1`.
    #[serde(rename = "127.0.0.1")]
    Loopback,
    /// `localhost`.
    #[serde(rename = "localhost")]
    Localhost,
}

impl McpRedirectHost {
    /// The host as written in the redirect URI.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Loopback => "127.0.0.1",
            Self::Localhost => "localhost",
        }
    }
}

/// `mcp.servers.<name>.oauth.flow`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum McpOAuthFlow {
    /// Authorization code with PKCE and a loopback (or pasted) redirect.
    Browser,
    /// Device code.
    Device,
}

/// `[mcp.servers.<name>.sampling]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct McpSamplingConfig {
    /// Default: on for `trust = "full"` servers, off for `untrusted`
    /// ones.
    pub enabled: Option<bool>,
    /// The model answering instead of the session's.
    pub model: Option<String>,
    /// When not empty, `model` must be one of these.
    pub allowed_models: Vec<String>,
    /// Requested answer sizes are capped at this.
    #[schemars(range(min = 1))]
    pub max_tokens_cap: u32,
    /// Bound on one sampling call.
    #[schemars(range(min = 1, max = 3_600))]
    pub timeout_secs: u64,
    /// Requests per sliding minute.
    #[schemars(range(min = 1))]
    pub max_rpm: u32,
    /// Tool rounds of one sampling exchange.
    pub max_tool_rounds: u32,
}

impl Default for McpSamplingConfig {
    fn default() -> Self {
        Self {
            enabled: None,
            model: None,
            allowed_models: Vec::new(),
            max_tokens_cap: 4_096,
            timeout_secs: 30,
            max_rpm: 10,
            max_tool_rounds: 5,
        }
    }
}

/// `[mcp.servers.<name>.elicitation]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct McpElicitationConfig {
    /// Route the server's form requests to the person.
    pub enabled: bool,
    /// Declined when unanswered this long.
    #[schemars(range(min = 1, max = 86_400))]
    pub timeout_secs: u64,
}

impl Default for McpElicitationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            timeout_secs: 300,
        }
    }
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

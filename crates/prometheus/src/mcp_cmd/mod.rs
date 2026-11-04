//! `prometheus mcp ...` (spec: plugin-system, MCP client servers; MCP
//! server mode; mcp CLI; MCP OAuth for HTTP servers; MCP catalog entries).
//!
//! `add` (with transport presets, probing the server and offering its
//! tool checklist), `list`, `remove` (alias `rm`), `test`, `tools`,
//! `login`, `reauth`, `catalog`, `install` and `status` manage the
//! profile's `[mcp.servers.<name>]` entries; `serve` offers Prometheus
//! tools and skills to MCP clients; with no subcommand on a terminal, an
//! interactive picker.

mod add;
pub(crate) mod catalog;
mod login;
mod picker;
pub mod serve;
mod tools;

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use clap::{Args, Subcommand, ValueEnum};
use pmt_config::ProfileScope;
use pmt_config::schema::McpTrust;
use pmt_core::error::{Error, codes};

use crate::context::Context;

/// `prometheus mcp` subcommands.
#[derive(Debug, Subcommand)]
pub enum McpCommand {
    /// Add a server: `--command <program> [--args ...]` for a local
    /// (stdio) server run in the sandbox, `--url <url>` for a streamable
    /// HTTP server, or `--preset <name>`; the server is probed and, on a
    /// terminal, its tool checklist offered. Its tools join sessions
    /// started afterwards.
    Add(AddArgs),
    /// Every configured server: transport, command or URL, trust and
    /// whether it is enabled.
    List,
    /// Remove a server.
    #[command(visible_alias = "rm")]
    Remove {
        /// The server's name.
        name: String,
    },
    /// Start a server and list its tools: handshake, tool count and
    /// latency (exit 69 when it does not answer).
    Test {
        /// The server's name.
        name: String,
    },
    /// Re-open a server's tool checklist; writes `tools.include` only when
    /// not every tool is selected.
    Tools {
        /// The server's name.
        name: String,
    },
    /// Sign in to an `auth = "oauth"` server (prints the authorization
    /// URL; paste the redirect URL when the browser cannot reach this
    /// machine).
    Login {
        /// The server's name.
        name: String,
        /// `browser` (default) or `device`.
        #[arg(long, value_enum)]
        flow: Option<Flow>,
    },
    /// Forget a server's sign-in and sign in again.
    Reauth {
        /// The server's name.
        name: String,
        /// `browser` (default) or `device`.
        #[arg(long, value_enum)]
        flow: Option<Flow>,
    },
    /// The MCP servers of the plugin catalog.
    Catalog,
    /// Install an MCP server from the catalog (disabled until you enable
    /// it).
    Install(catalog::InstallArgs),
    /// Start every enabled server and show its state, tool count and why
    /// it is unavailable.
    Status,
    /// Serve this profile's `mcp_server.expose_toolsets` (default: the
    /// read-only skill tools) as MCP tools and its skills as MCP prompts
    /// and resources, on stdio or streamable HTTP.
    Serve(serve::ServeArgs),
}

/// `--flow`.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Flow {
    /// Authorization code with PKCE in a browser.
    Browser,
    /// Device code.
    Device,
}

/// `mcp add` flags.
#[derive(Debug, Args)]
pub struct AddArgs {
    /// The server's name (letters, digits, `_` and `-`); its tools are
    /// named `mcp__<name>__<tool>`.
    pub name: String,
    /// The streamable HTTP endpoint.
    #[arg(long, conflicts_with_all = ["command", "preset"], required_unless_present_any = ["command", "preset"])]
    pub url: Option<String>,
    /// The program of a local (stdio) server.
    #[arg(long)]
    pub command: Option<String>,
    /// The program's arguments.
    #[arg(long, num_args = 1.., allow_hyphen_values = true)]
    pub args: Vec<String>,
    /// A transport preset (`codex`: `codex mcp-server`); explicit flags
    /// win over it.
    #[arg(long, value_enum)]
    pub preset: Option<add::Preset>,
    /// An environment variable of a stdio server (`KEY=value`; repeat).
    #[arg(long = "env", value_name = "KEY=VALUE")]
    pub env: Vec<String>,
    /// A secrets-store name passed to a stdio server as a variable of the
    /// same name (repeat).
    #[arg(long = "env-secret", value_name = "NAME")]
    pub env_secrets: Vec<String>,
    /// How an HTTP server authenticates: `header` (the `--auth-secret`
    /// bearer token) or `oauth` (sign in with `prometheus mcp login`).
    #[arg(long, value_enum, requires = "url")]
    pub auth: Option<add::Auth>,
    /// The secrets-store name of an HTTP server's bearer token.
    #[arg(long, value_name = "NAME", requires = "url")]
    pub auth_secret: Option<String>,
    /// `untrusted` (default): tools that are not read-only need approval.
    #[arg(long, value_enum)]
    pub trust: Option<Trust>,
}

/// `--trust`.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Trust {
    /// Trusted like built-in tools.
    Full,
    /// Tools that are not read-only need approval.
    Untrusted,
}

/// Runs `prometheus mcp`.
///
/// # Errors
///
/// Configuration, secret and server errors.
pub fn run(command: Option<&McpCommand>, ctx: &Context<'_>) -> Result<u8, Error> {
    let Some(command) = command else {
        return picker::run(ctx);
    };
    match command {
        McpCommand::Add(args) => add::add(ctx, args),
        McpCommand::List => Ok(list(&ctx.scope()?, ctx.json)),
        McpCommand::Remove { name } => remove(ctx, name),
        McpCommand::Test { name } => test(&ctx.scope()?, name),
        McpCommand::Tools { name } => tools::run(ctx, name),
        McpCommand::Login { name, flow } => login::login(ctx, name, *flow, false),
        McpCommand::Reauth { name, flow } => login::login(ctx, name, *flow, true),
        McpCommand::Catalog => catalog::list(ctx),
        McpCommand::Install(args) => catalog::install(ctx, args),
        McpCommand::Status => status(&ctx.scope()?, ctx.json),
        McpCommand::Serve(args) => serve::run(args, ctx),
    }
}

/// `value` as JSON text, which is also its TOML value (a string or a
/// list of strings).
fn json(value: &impl serde::Serialize) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

pub(super) fn exists(name: &str) -> Error {
    Error::new(
        &codes::MCP_EXISTS,
        format!("MCP server '{name}' is already configured"),
    )
    .with_hint(format!("remove it first: prometheus mcp remove {name}"))
}

pub(super) fn not_found(name: &str) -> Error {
    Error::new(
        &codes::MCP_NOT_FOUND,
        format!("no MCP server named '{name}' is configured"),
    )
    .with_hint("see the configured servers with: prometheus mcp list")
}

fn remove(ctx: &Context<'_>, name: &str) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    if !scope.config().mcp.servers.contains_key(name) {
        return Err(not_found(name));
    }
    scope
        .profile()
        .config_file()
        .unset(&format!("mcp.servers.{name}"))?;
    println!("removed MCP server '{name}'; sessions started from now no longer get its tools");
    Ok(0)
}

fn list(scope: &ProfileScope, json: bool) -> u8 {
    let config = scope.config();
    if json {
        let servers: Vec<serde_json::Value> = config
            .mcp
            .servers
            .iter()
            .map(|(name, server)| {
                serde_json::json!({
                    "name": name,
                    "transport": server.transport.as_str(),
                    "target": crate::mcp_host::target(server),
                    "trust": trust(server.trust),
                    "enabled": server.enabled,
                })
            })
            .collect();
        println!("{}", serde_json::json!({"servers": servers}));
        return 0;
    }
    if config.mcp.servers.is_empty() {
        println!("no MCP servers are configured (add one with: prometheus mcp add)");
        return 0;
    }
    for (name, server) in &config.mcp.servers {
        let enabled = if server.enabled { "" } else { " (disabled)" };
        println!(
            "{name}  {}  {}  trust {}{enabled}",
            server.transport.as_str(),
            crate::mcp_host::target(server),
            trust(server.trust)
        );
    }
    0
}

fn trust(trust: McpTrust) -> &'static str {
    match trust {
        McpTrust::Full => "full",
        McpTrust::Untrusted => "untrusted",
    }
}

pub(super) fn runtime() -> Result<tokio::runtime::Runtime, Error> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|e| Error::new(&codes::TASK_PANICKED, format!("no async runtime: {e}")))
}

fn test(scope: &ProfileScope, name: &str) -> Result<u8, Error> {
    if !scope.config().mcp.servers.contains_key(name) {
        return Err(not_found(name));
    }
    let manager = crate::mcp_host::reload(scope);
    let Some(server) = manager.server(name).cloned() else {
        // Configured but disabled, skipped for an error, or safe mode.
        let why = crate::mcp_host::skipped(scope)
            .into_iter()
            .find(|(n, _)| n == name)
            .map_or_else(
                || "is disabled".to_owned(),
                |(_, why)| format!("is skipped: {why}"),
            );
        return Err(Error::new(
            &codes::MCP_UNREACHABLE,
            format!("MCP server '{name}' {why}"),
        ));
    };
    let started = Instant::now();
    let outcome = runtime()?.block_on(server.connect());
    let latency = started.elapsed();
    match outcome {
        Ok(()) => {
            let tools = server.tools();
            let admitted = tools
                .iter()
                .filter(|t| server.settings().filter.admits(&t.name))
                .count();
            println!(
                "MCP server '{name}' answered in {}: {} tools ({admitted} registered)",
                millis(latency),
                tools.len()
            );
            Ok(0)
        }
        Err(reason) => Err(Error::new(
            &codes::MCP_UNREACHABLE,
            format!("MCP server '{name}' did not answer: {reason}"),
        )),
    }
}

fn millis(duration: Duration) -> String {
    format!("{} ms", duration.as_millis())
}

fn status(scope: &ProfileScope, json: bool) -> Result<u8, Error> {
    let manager = crate::mcp_host::manager(scope);
    runtime()?.block_on(manager.discover());
    let rows = crate::mcp_host::rows(scope);
    if json {
        println!("{}", serde_json::json!({"servers": rows}));
        return Ok(0);
    }
    if rows.is_empty() {
        println!("no MCP servers are configured");
        return Ok(0);
    }
    let mut text = String::new();
    for row in rows {
        let _ = write!(text, "{}  {}  {}", row.name, row.transport, row.state);
        if let Some(reason) = &row.reason {
            let _ = write!(text, ": {reason}");
        }
        let _ = writeln!(text, "  {} tools", row.tools);
    }
    print!("{text}");
    Ok(0)
}

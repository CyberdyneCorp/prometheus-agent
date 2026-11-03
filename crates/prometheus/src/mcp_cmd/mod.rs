//! `prometheus mcp ...` (spec: plugin-system, MCP client servers; MCP
//! server mode; mcp CLI).
//!
//! `add`, `list`, `remove` (alias `rm`), `test` and `status` manage the
//! profile's `[mcp.servers.<name>]` entries; `serve` offers Prometheus
//! tools and skills to MCP clients. The interactive picker, transport
//! presets, OAuth logins, tool checklists and the catalog arrive with the
//! rest of the `mcp` CLI.

pub mod serve;

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
    /// (stdio) server run in the sandbox, or `--url <url>` for a
    /// streamable HTTP server. Its tools join sessions started afterwards.
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
    /// Start every enabled server and show its state, tool count and why
    /// it is unavailable.
    Status,
    /// Serve this profile's `mcp_server.expose_toolsets` (default: the
    /// read-only skill tools) as MCP tools and its skills as MCP prompts
    /// and resources, on stdio or streamable HTTP.
    Serve(serve::ServeArgs),
}

/// `mcp add` flags.
#[derive(Debug, Args)]
pub struct AddArgs {
    /// The server's name (letters, digits, `_` and `-`); its tools are
    /// named `mcp__<name>__<tool>`.
    pub name: String,
    /// The streamable HTTP endpoint.
    #[arg(long, conflicts_with = "command", required_unless_present = "command")]
    pub url: Option<String>,
    /// The program of a local (stdio) server.
    #[arg(long)]
    pub command: Option<String>,
    /// The program's arguments.
    #[arg(long, num_args = 1.., allow_hyphen_values = true, requires = "command")]
    pub args: Vec<String>,
    /// An environment variable of a stdio server (`KEY=value`; repeat).
    #[arg(long = "env", value_name = "KEY=VALUE", requires = "command")]
    pub env: Vec<String>,
    /// A secrets-store name passed to a stdio server as a variable of the
    /// same name (repeat).
    #[arg(long = "env-secret", value_name = "NAME", requires = "command")]
    pub env_secrets: Vec<String>,
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
pub fn run(command: &McpCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    match command {
        McpCommand::Add(args) => add(ctx, args),
        McpCommand::List => Ok(list(&ctx.scope()?, ctx.json)),
        McpCommand::Remove { name } => remove(ctx, name),
        McpCommand::Test { name } => test(&ctx.scope()?, name),
        McpCommand::Status => status(&ctx.scope()?, ctx.json),
        McpCommand::Serve(args) => serve::run(args, ctx),
    }
}

fn add(ctx: &Context<'_>, args: &AddArgs) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    if !pmt_config::mcp_check::is_server_name(&args.name) {
        return Err(Error::new(
            &codes::CLI_USAGE,
            format!(
                "invalid MCP server name '{}': use 1-64 letters, digits, '_' or '-'",
                args.name
            ),
        ));
    }
    if scope.config().mcp.servers.contains_key(&args.name) {
        return Err(exists(&args.name));
    }
    let pairs = pairs(args)?;
    let borrowed: Vec<(&str, &str)> = pairs
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    scope.profile().config_file().set_all(&borrowed)?;
    println!(
        "added MCP server '{}'; its tools join sessions started from now (check it with: prometheus mcp test {})",
        args.name, args.name
    );
    Ok(0)
}

/// The `config.toml` keys `mcp add` writes.
fn pairs(args: &AddArgs) -> Result<Vec<(String, String)>, Error> {
    let key = |field: &str| format!("mcp.servers.{}.{field}", args.name);
    let mut pairs = Vec::new();
    match (&args.url, &args.command) {
        (Some(url), _) => {
            pairs.push((key("transport"), "\"http\"".to_owned()));
            pairs.push((key("url"), json(url)));
            if let Some(secret) = &args.auth_secret {
                pairs.push((key("auth_secret"), json(secret)));
            }
        }
        (None, Some(command)) => {
            pairs.push((key("transport"), "\"stdio\"".to_owned()));
            pairs.push((key("command"), json(command)));
            if !args.args.is_empty() {
                pairs.push((key("args"), json(&args.args)));
            }
            for pair in &args.env {
                let Some((name, value)) = pair.split_once('=') else {
                    return Err(Error::new(
                        &codes::CLI_USAGE,
                        format!("--env {pair}: expected KEY=VALUE"),
                    ));
                };
                pairs.push((key(&format!("env.{name}")), json(&value)));
            }
            if !args.env_secrets.is_empty() {
                pairs.push((key("env_secrets"), json(&args.env_secrets)));
            }
        }
        (None, None) => {
            return Err(Error::new(&codes::CLI_USAGE, "give --command or --url"));
        }
    }
    if let Some(trust) = args.trust {
        let value = match trust {
            Trust::Full => "\"full\"",
            Trust::Untrusted => "\"untrusted\"",
        };
        pairs.push((key("trust"), value.to_owned()));
    }
    Ok(pairs)
}

/// `value` as JSON text, which is also its TOML value (a string or a
/// list of strings).
fn json(value: &impl serde::Serialize) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

fn exists(name: &str) -> Error {
    Error::new(
        &codes::MCP_EXISTS,
        format!("MCP server '{name}' is already configured"),
    )
    .with_hint(format!("remove it first: prometheus mcp remove {name}"))
}

fn not_found(name: &str) -> Error {
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

fn runtime() -> Result<tokio::runtime::Runtime, Error> {
    tokio::runtime::Builder::new_current_thread()
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
        // Configured but disabled (or safe mode).
        return Err(Error::new(
            &codes::MCP_UNREACHABLE,
            format!("MCP server '{name}' is disabled"),
        ));
    };
    let started = Instant::now();
    let outcome = runtime()?.block_on(server.ensure());
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

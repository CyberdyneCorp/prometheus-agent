//! `prometheus mcp serve [--stdio | --http <addr>]` (spec: plugin-system,
//! MCP server mode).
//!
//! The server is a non-interactive session of the selected profile (a
//! stored session tagged `mcp-serve`) exposing `mcp_server.expose_toolsets`
//! and the profile's skills. Over `--http` it binds a loopback address
//! unless `mcp_server.allow_remote` is set, and every request needs the
//! bearer token `MCP_SERVER_TOKEN` from the profile's secrets store,
//! generated and stored on first use.

use std::net::SocketAddr;
use std::sync::Arc;

use clap::Args;
use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_store::{NewSession, Source};

use crate::chat::session::Factory;
use crate::chat::session::model::RouteFlags;
use crate::context::Context;

/// The secrets-store name of the HTTP bearer token.
pub(super) const TOKEN_SECRET: &str = "MCP_SERVER_TOKEN";

/// The session source tag of the server's session.
const SOURCE_TAG: &str = "mcp-serve";

/// `mcp serve` flags.
#[derive(Debug, Args)]
pub struct ServeArgs {
    /// Serve on stdin and stdout (the default).
    #[arg(long, conflicts_with = "http")]
    pub stdio: bool,
    /// Serve streamable HTTP on this address (`127.0.0.1:8765`); a
    /// non-loopback address needs `mcp_server.allow_remote = true`.
    #[arg(long, value_name = "ADDR")]
    pub http: Option<String>,
}

/// Runs `mcp serve` until the client leaves (stdio) or Ctrl-C (HTTP).
///
/// # Errors
///
/// A bad or refused address, configuration, store and secrets errors.
pub(super) fn run(args: &ServeArgs, ctx: &Context<'_>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let address = args
        .http
        .as_deref()
        .map(|a| address(&scope, a))
        .transpose()?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::new(&codes::TASK_PANICKED, format!("no async runtime: {e}")))?;
    runtime.block_on(async {
        let host = host(scope.clone(), ctx).await?;
        match address {
            None => pmt_mcp::serve::stdio(host)
                .await
                .map_err(|e| Error::new(&codes::MCP_UNREACHABLE, e)),
            Some(address) => http(&scope, host, address).await,
        }
    })?;
    Ok(0)
}

/// `addr`, refused when it is not loopback and remote serving is off.
fn address(scope: &ProfileScope, addr: &str) -> Result<SocketAddr, Error> {
    let address: SocketAddr = addr.parse().map_err(|e| {
        Error::new(
            &codes::CLI_USAGE,
            format!("--http {addr}: not an address ({e}); e.g. 127.0.0.1:8765"),
        )
    })?;
    if !address.ip().is_loopback() && !scope.config().mcp_server.allow_remote {
        return Err(Error::new(
            &codes::MCP_REMOTE_REFUSED,
            format!("refusing to serve MCP on {address}: it is not a loopback address"),
        )
        .with_hint("set mcp_server.allow_remote = true in config.toml to allow it"));
    }
    Ok(address)
}

async fn host(
    scope: ProfileScope,
    ctx: &Context<'_>,
) -> Result<Arc<dyn pmt_mcp::serve::Host>, Error> {
    let options = crate::chat::session::Options {
        interactive: false,
        cwd: std::env::current_dir().unwrap_or_else(|_| scope.dir().to_path_buf()),
        source_tag: Some(SOURCE_TAG.to_owned()),
        ..crate::chat::session::Options::default()
    };
    let flags = RouteFlags {
        provider: None,
        model: None,
        mock_script: None,
    };
    let factory = Factory::new(scope, ctx.launch, &flags, None, options)?;
    let mut session = NewSession::new(Source::Cli);
    session.source_tag = Some(SOURCE_TAG.to_owned());
    session.cwd = Some(factory.options.cwd.to_string_lossy().into_owned());
    let id = factory.store.create_session(&session)?.id;
    let host = factory.serve_host(id).await?;
    // The factory's shared tools live as long as the server.
    let factory = Arc::new(factory);
    Ok(Arc::new(Served {
        host,
        _factory: factory,
    }))
}

/// The host and the factory its session belongs to.
struct Served {
    host: crate::chat::session::serve::ServeHost,
    _factory: Arc<Factory>,
}

impl pmt_mcp::serve::Host for Served {
    fn tools(&self) -> Vec<pmt_mcp::serve::ExposedTool> {
        self.host.tools()
    }

    fn call(
        &self,
        name: String,
        arguments: serde_json::Value,
    ) -> pmt_mcp::serve::HostFuture<'_, pmt_tools::ToolResult> {
        self.host.call(name, arguments)
    }

    fn skills(&self) -> Vec<pmt_mcp::serve::SkillEntry> {
        self.host.skills()
    }

    fn skill(&self, name: String) -> pmt_mcp::serve::HostFuture<'_, Result<String, String>> {
        self.host.skill(name)
    }
}

/// The HTTP bearer token, generated and stored on first use.
fn token(scope: &ProfileScope) -> Result<String, Error> {
    let secrets = scope.secrets().store();
    if let Some(token) = secrets.get(TOKEN_SECRET)? {
        return Ok(token.expose().to_owned());
    }
    let token = pmt_providers::auth::pkce::random_token(32)?;
    secrets.set(TOKEN_SECRET, &token)?;
    eprintln!(
        "generated the MCP bearer token and stored it as {TOKEN_SECRET} (show it with: prometheus secrets get {TOKEN_SECRET} --reveal)"
    );
    Ok(token.expose().to_owned())
}

async fn http(
    scope: &ProfileScope,
    host: Arc<dyn pmt_mcp::serve::Host>,
    address: SocketAddr,
) -> Result<(), Error> {
    let token = token(scope)?;
    let listener = tokio::net::TcpListener::bind(address).await.map_err(|e| {
        Error::new(
            &codes::MCP_UNREACHABLE,
            format!("cannot listen on {address}: {e}"),
        )
    })?;
    let bound = listener.local_addr().unwrap_or(address);
    eprintln!("serving MCP on http://{bound}/ (bearer token {TOKEN_SECRET}); Ctrl-C stops");
    let remote = !bound.ip().is_loopback();
    tokio::select! {
        served = pmt_mcp::serve::http::serve(host, listener, token, remote) => {
            served.map_err(|e| Error::new(&codes::MCP_UNREACHABLE, e))
        }
        _ = tokio::signal::ctrl_c() => Ok(()),
    }
}

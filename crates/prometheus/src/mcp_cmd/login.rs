//! `prometheus mcp login|reauth <name>` (spec: plugin-system, MCP OAuth for
//! HTTP servers; mcp CLI).
//!
//! The login runs in the foreground: it prints the authorization URL,
//! opens a browser only when stdout is a terminal and the session is not
//! remote (SSH), and accepts the redirect URL pasted on stdin. `reauth`
//! forgets the stored sign-in first.

use pmt_config::schema::{McpAuth, McpOAuthFlow, McpTransport};
use pmt_core::error::{Error, codes};

use super::{Flow, not_found, runtime};
use crate::context::Context;
use crate::mcp_host::oauth::{self, login::LoginOptions};

pub(super) fn login(
    ctx: &Context<'_>,
    name: &str,
    flow: Option<Flow>,
    forget: bool,
) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let Some(server) = scope.config().mcp.servers.get(name).cloned() else {
        return Err(not_found(name));
    };
    if server.transport != McpTransport::Http || server.auth != Some(McpAuth::Oauth) {
        return Err(Error::new(
            &codes::CONFIG_INVALID,
            format!("MCP server '{name}' does not sign in with OAuth"),
        )
        .with_hint(format!(
            "set mcp.servers.{name}.auth = \"oauth\" on an http server"
        )));
    }
    if forget {
        oauth::forget(&scope, name).map_err(|why| Error::new(&codes::MCP_LOGIN_FAILED, why))?;
    }
    let flow = match flow {
        Some(Flow::Device) => McpOAuthFlow::Device,
        Some(Flow::Browser) => McpOAuthFlow::Browser,
        None => server.oauth.flow,
    };
    let remote =
        std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some();
    let options = LoginOptions {
        flow,
        open_browser: crate::prompt::stdout_is_tty() && !remote,
    };
    runtime()?.block_on(oauth::login::login(&scope, name, &server, options))?;
    // The server set restarts so the next start uses the new token.
    let _ = crate::mcp_host::reload(&scope);
    println!("signed in to MCP server '{name}'; its tools join sessions started from now");
    Ok(0)
}

//! Catalog MCP targets of `manage_connections` (spec: connectors,
//! `manage_connections` tool; plugin-system, MCP catalog entries): every
//! action runs after the user approved it (the tool's guard).

use pmt_config::ProfileScope;
use pmt_config::schema::{McpAuth, McpTransport};
use pmt_tools::connectors::manage::McpTargets;
use pmt_tools::search::RemoteFuture;
use serde_json::{Value, json};

/// The profile's catalog and configured MCP servers.
pub(super) struct CatalogTargets {
    scope: ProfileScope,
    /// The session shows cards: a sign-in may open a browser here.
    attended: bool,
}

impl CatalogTargets {
    pub(super) fn new(scope: ProfileScope, attended: bool) -> Self {
        Self { scope, attended }
    }

    fn configured(&self, name: &str) -> Option<pmt_config::schema::McpServerConfig> {
        self.scope.config().mcp.servers.get(name).cloned()
    }
}

impl McpTargets for CatalogTargets {
    fn install<'a>(&'a self, name: &'a str) -> RemoteFuture<'a, Result<String, String>> {
        Box::pin(async move {
            let scope = self.scope.clone();
            let id = name.to_owned();
            tokio::task::spawn_blocking(move || {
                crate::mcp_cmd::catalog::install_unattended(&scope, &id)
            })
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.message().to_owned())
        })
    }

    fn enable<'a>(&'a self, name: &'a str) -> RemoteFuture<'a, Result<String, String>> {
        Box::pin(async move {
            if self.configured(name).is_none() {
                return Err(format!(
                    "no MCP server '{name}' is configured; install it first"
                ));
            }
            self.scope
                .profile()
                .config_file()
                .set_all(&[(&format!("mcp.servers.{name}.enabled"), "true")])
                .map_err(|e| e.message().to_owned())?;
            Ok(format!(
                "enabled MCP server '{name}'; its tools join sessions started from now"
            ))
        })
    }

    fn authorize<'a>(&'a self, name: &'a str) -> RemoteFuture<'a, Result<String, String>> {
        Box::pin(async move {
            let Some(server) = self.configured(name) else {
                return Err(format!("no MCP server '{name}' is configured"));
            };
            if server.transport != McpTransport::Http || server.auth != Some(McpAuth::Oauth) {
                return Err(format!("MCP server '{name}' does not sign in with OAuth"));
            }
            if !self.attended {
                return Err(format!(
                    "signing in needs the user at their computer: ask them to run `prometheus mcp login {name}`"
                ));
            }
            let options = crate::mcp_host::oauth::login::LoginOptions {
                flow: server.oauth.flow,
                open_browser: crate::prompt::stdout_is_tty(),
            };
            crate::mcp_host::oauth::login::login(&self.scope, name, &server, options)
                .await
                .map_err(|e| e.message().to_owned())?;
            let _ = crate::mcp_host::reload(&self.scope);
            Ok(format!("signed in to MCP server '{name}'"))
        })
    }

    fn status(&self, name: &str) -> Value {
        let catalog = crate::plugins_host::catalog::current(&self.scope, false);
        let configured = self.configured(name);
        json!({
            "name": name,
            "in_catalog": catalog.find_mcp(name).is_some(),
            "installed": configured.is_some(),
            "enabled": configured.is_some_and(|s| s.enabled),
        })
    }
}

//! The binary's side of the MCP client (spec: plugin-system, MCP client
//! servers; tool-system, MCP runtime safety; Tool surface is frozen for
//! the session).
//!
//! Each profile has one set of MCP servers per process ([`Manager`]),
//! shared by its sessions, built from `[mcp]` and replaced as a whole when
//! that section (or safe mode) changes or on `/mcp reload`. Servers start
//! when a session needs them, never at process start; a session's tools
//! come from the servers' tool lists as they were when it started.
//! Safe mode starts none.

mod launch;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use pmt_config::ProfileScope;
use pmt_config::schema::McpServerConfig;
use pmt_mcp::naming::Filter;
use pmt_mcp::server::State;
use pmt_mcp::{Manager, ServerEntry, Settings};

/// A profile's server set and the configuration it was built from.
struct Entry {
    fingerprint: String,
    manager: Arc<Manager>,
}

fn cache() -> &'static Mutex<HashMap<PathBuf, Entry>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, Entry>>> = OnceLock::new();
    CACHE.get_or_init(Mutex::default)
}

/// What the server set depends on.
fn fingerprint(scope: &ProfileScope) -> String {
    let config = scope.config();
    format!(
        "{}|{}|{:?}|{:?}",
        serde_json::to_string(&config.mcp).unwrap_or_default(),
        config.plugins.safe_mode,
        config.network,
        config.security.sandbox,
    )
}

/// The profile's MCP servers, rebuilt when `[mcp]` changed since they
/// were built (the old ones stop).
#[must_use]
pub fn manager(scope: &ProfileScope) -> Arc<Manager> {
    let fingerprint = fingerprint(scope);
    let mut cache = cache().lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(entry) = cache.get(scope.dir())
        && entry.fingerprint == fingerprint
    {
        return Arc::clone(&entry.manager);
    }
    let manager = Arc::new(build(scope));
    if let Some(old) = cache.insert(
        scope.dir().to_path_buf(),
        Entry {
            fingerprint,
            manager: Arc::clone(&manager),
        },
    ) {
        old.manager.stop();
    }
    manager
}

/// `/mcp reload`: the profile's servers rebuilt from its configuration now
/// (every server restarts when next needed).
#[must_use]
pub fn reload(scope: &ProfileScope) -> Arc<Manager> {
    if let Some(old) = cache()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(scope.dir())
    {
        old.manager.stop();
    }
    manager(scope)
}

fn build(scope: &ProfileScope) -> Manager {
    let config = scope.config();
    let entries = if config.plugins.safe_mode {
        Vec::new()
    } else {
        config
            .mcp
            .servers
            .iter()
            .filter(|(_, server)| server.enabled)
            .map(|(name, server)| entry(scope, name, server))
            .collect()
    };
    let concurrency = usize::try_from(config.mcp.discovery_concurrency).unwrap_or(4);
    Manager::new(entries, concurrency)
}

fn entry(scope: &ProfileScope, name: &str, server: &McpServerConfig) -> ServerEntry {
    let mcp = &scope.config().mcp;
    ServerEntry {
        name: name.to_owned(),
        settings: Settings {
            transport: server.transport.as_str(),
            trust: server.trust,
            filter: Filter::of(&server.tools),
            call_timeout: Duration::from_secs(server.timeout_secs.unwrap_or(mcp.call_timeout_secs)),
            connect_timeout: Duration::from_secs(
                server
                    .connect_timeout_secs
                    .unwrap_or(mcp.connect_timeout_secs),
            ),
        },
        launcher: launch::launcher(scope, name, server),
    }
}

/// One configured server as `mcp list` and `mcp status` show it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Row {
    /// The configured name.
    pub name: String,
    /// `stdio` or `http`.
    pub transport: &'static str,
    /// The command line or URL.
    pub target: String,
    /// `connected`, `not connected`, `unavailable`, `disabled` or
    /// `skipped (safe mode)`.
    pub state: String,
    /// Why it is unavailable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Tools it listed at its last start.
    pub tools: usize,
}

/// Every configured server with its state in this process.
#[must_use]
pub fn rows(scope: &ProfileScope) -> Vec<Row> {
    let config = scope.config();
    let statuses = manager(scope).statuses();
    config
        .mcp
        .servers
        .iter()
        .map(|(name, server)| {
            let status = statuses.iter().find(|s| &s.name == name);
            let (state, reason) = match status.map(|s| &s.state) {
                _ if !server.enabled => ("disabled".to_owned(), None),
                None => ("skipped (safe mode)".to_owned(), None),
                Some(State::Connected) => ("connected".to_owned(), None),
                Some(State::NotConnected) => ("not connected".to_owned(), None),
                Some(State::Unavailable(why)) => ("unavailable".to_owned(), Some(why.clone())),
            };
            Row {
                name: name.clone(),
                transport: server.transport.as_str(),
                target: target(server),
                state,
                reason,
                tools: status.map_or(0, |s| s.tools),
            }
        })
        .collect()
}

/// The command line (`command args...`) or URL of `server`.
#[must_use]
pub fn target(server: &McpServerConfig) -> String {
    match server.transport {
        pmt_config::schema::McpTransport::Http => server.url.clone(),
        pmt_config::schema::McpTransport::Stdio => {
            let mut words = vec![server.command.clone()];
            words.extend(server.args.iter().cloned());
            words.join(" ")
        }
    }
}

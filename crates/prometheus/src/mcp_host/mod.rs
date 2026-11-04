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
pub mod oauth;
mod placeholders;
pub mod sessions;
mod tls;

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};
use std::time::Duration;

use pmt_config::ProfileScope;
use pmt_config::schema::{McpServerConfig, McpTransport};
use pmt_mcp::costs::Costs;
use pmt_mcp::naming::Filter;
use pmt_mcp::requests::{ElicitationPolicy, SamplingPolicy};
use pmt_mcp::server::{Recycle, State};
use pmt_mcp::{Manager, Settings};

/// A profile's server set and the configuration it was built from.
struct Entry {
    fingerprint: String,
    manager: Arc<Manager>,
    /// Each server's own fingerprint (a changed one is rebuilt).
    servers: BTreeMap<String, String>,
    /// Servers skipped for a configuration error, with it.
    skipped: Vec<(String, String)>,
}

fn cache() -> &'static Mutex<HashMap<PathBuf, Entry>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, Entry>>> = OnceLock::new();
    CACHE.get_or_init(Mutex::default)
}

/// What every server depends on besides its own entry.
fn shared_fingerprint(scope: &ProfileScope) -> String {
    let config = scope.config();
    let mcp = &config.mcp;
    format!(
        "{}|{}|{}|{:?}|{:?}",
        mcp.call_timeout_secs,
        mcp.connect_timeout_secs,
        config.plugins.safe_mode,
        config.network,
        config.security.sandbox,
    )
}

/// What the server set depends on.
fn fingerprint(scope: &ProfileScope) -> String {
    format!(
        "{}|{}",
        serde_json::to_string(&scope.config().mcp).unwrap_or_default(),
        shared_fingerprint(scope)
    )
}

/// What server `server` depends on.
fn server_fingerprint(scope: &ProfileScope, server: &McpServerConfig) -> String {
    format!(
        "{}|{}",
        serde_json::to_string(server).unwrap_or_default(),
        shared_fingerprint(scope)
    )
}

/// The profile's MCP servers. When `[mcp]` changed since they were built,
/// the set is rebuilt keeping every unchanged server (connection
/// included): removed, disabled and changed servers stop. With
/// `mcp.auto_reload_on_config_change = false` the old set stays until
/// `/mcp reload`.
#[must_use]
pub fn manager(scope: &ProfileScope) -> Arc<Manager> {
    let fingerprint = fingerprint(scope);
    let mut cache = cache().lock().unwrap_or_else(PoisonError::into_inner);
    let old = cache.get(scope.dir());
    if let Some(entry) = old
        && (entry.fingerprint == fingerprint || !scope.config().mcp.auto_reload_on_config_change)
    {
        return Arc::clone(&entry.manager);
    }
    let entry = build(scope, fingerprint, old);
    let manager = Arc::clone(&entry.manager);
    if let Some(old) = cache.insert(scope.dir().to_path_buf(), entry) {
        let kept: Vec<&Arc<pmt_mcp::Server>> = manager.servers().iter().collect();
        for server in old.manager.servers() {
            if !kept.iter().any(|k| Arc::ptr_eq(k, server)) {
                server.stop();
            }
        }
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

/// The daemon saw `scope`'s configuration change: the server set follows
/// it now (when `mcp.auto_reload_on_config_change`), and servers that are
/// new start in the background, a failed first connect retried after
/// 30 s, doubling to 10 minutes. Sessions already running keep their
/// tools; new sessions get the new set.
pub fn config_changed(scope: &ProfileScope) {
    if !scope.config().mcp.auto_reload_on_config_change {
        return;
    }
    let before: Vec<Arc<pmt_mcp::Server>> = cache()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(scope.dir())
        .map(|e| e.manager.servers().to_vec())
        .unwrap_or_default();
    let manager = manager(scope);
    for server in manager.servers() {
        if !before.iter().any(|b| Arc::ptr_eq(b, server)) {
            pmt_mcp::runtime::handle().spawn(connect_with_cooldown(Arc::downgrade(server)));
        }
    }
}

/// The first connect of a server the configuration added.
async fn connect_with_cooldown(server: std::sync::Weak<pmt_mcp::Server>) {
    let mut cooldown = FIRST_CONNECT_COOLDOWN;
    loop {
        let Some(this) = server.upgrade() else { return };
        match this.ensure().await {
            Ok(()) => return,
            Err(why) => {
                tracing::warn!(server = %this.name(), "MCP server added by a configuration change did not start: {why}; retrying in {} s", cooldown.as_secs());
            }
        }
        drop(this);
        tokio::time::sleep(cooldown).await;
        cooldown = (cooldown * 2).min(FIRST_CONNECT_COOLDOWN_MAX);
    }
}

/// The first retry of a failed first connect after a configuration change.
const FIRST_CONNECT_COOLDOWN: Duration = Duration::from_secs(30);
/// The longest wait between those retries.
const FIRST_CONNECT_COOLDOWN_MAX: Duration = Duration::from_mins(10);

/// Servers of the profile skipped for a configuration error, with it.
#[must_use]
pub fn skipped(scope: &ProfileScope) -> Vec<(String, String)> {
    let _ = manager(scope);
    cache()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(scope.dir())
        .map(|entry| entry.skipped.clone())
        .unwrap_or_default()
}

fn build(scope: &ProfileScope, fingerprint: String, old: Option<&Entry>) -> Entry {
    let config = scope.config();
    let mut servers = Vec::new();
    let mut fingerprints = BTreeMap::new();
    let mut skipped = Vec::new();
    let host: Arc<dyn pmt_mcp::ClientHost> = Arc::new(sessions::SessionsHost {
        profile_dir: scope.dir().to_path_buf(),
    });
    if !config.plugins.safe_mode {
        for (name, server) in config.mcp.servers.iter().filter(|(_, s)| s.enabled) {
            let print = server_fingerprint(scope, server);
            let reused = old
                .filter(|o| o.servers.get(name) == Some(&print))
                .and_then(|o| o.manager.server(name).cloned());
            if let Some(reused) = reused {
                servers.push(reused);
                fingerprints.insert(name.clone(), print);
                continue;
            }
            match settings(scope, name, server) {
                Ok(settings) => {
                    servers.push(new_server(scope, name, server, settings, &host));
                    fingerprints.insert(name.clone(), print);
                }
                Err(why) => {
                    tracing::error!("MCP server '{name}' skipped: mcp.servers.{name}.{why}");
                    skipped.push((name.clone(), format!("mcp.servers.{name}.{why}")));
                }
            }
        }
    }
    let concurrency = usize::try_from(config.mcp.discovery_concurrency).unwrap_or(4);
    Entry {
        fingerprint,
        manager: Arc::new(Manager::from_servers(servers, concurrency)),
        servers: fingerprints,
        skipped,
    }
}

/// How server `name` is used.
/// Server `name`, whose launcher attributes its egress refusals to the
/// session of the call in flight (known once the server exists).
fn new_server(
    scope: &ProfileScope,
    name: &str,
    server: &McpServerConfig,
    settings: Settings,
    host: &Arc<dyn pmt_mcp::ClientHost>,
) -> Arc<pmt_mcp::Server> {
    let made: Arc<OnceLock<Weak<pmt_mcp::Server>>> = Arc::default();
    let slot = Arc::clone(&made);
    let caller: crate::egress::Caller = Arc::new(move || slot.get()?.upgrade()?.caller());
    let created = Arc::new(pmt_mcp::Server::new(
        name,
        settings,
        launch::launcher(scope, name, server, caller),
        Arc::clone(host),
    ));
    let _ = made.set(Arc::downgrade(&created));
    created
}

fn settings(
    scope: &ProfileScope,
    name: &str,
    server: &McpServerConfig,
) -> Result<Settings, String> {
    let mcp = &scope.config().mcp;
    let costs = Costs::parse(&server.tool_costs, server.spend_limit, &server.cost_unit)?;
    let stdio = server.transport == McpTransport::Stdio;
    let seconds = |secs: u64| (stdio && secs > 0).then(|| Duration::from_secs(secs));
    Ok(Settings {
        transport: server.transport.as_str(),
        trust: server.trust,
        filter: Filter::of(&server.tools),
        call_timeout: Duration::from_secs(server.timeout_secs.unwrap_or(mcp.call_timeout_secs)),
        connect_timeout: Duration::from_secs(
            server
                .connect_timeout_secs
                .unwrap_or(mcp.connect_timeout_secs),
        ),
        sampling: SamplingPolicy::of(&server.sampling, server.trust),
        elicitation: ElicitationPolicy::of(&server.elicitation),
        costs,
        recycle: Recycle {
            idle: seconds(server.idle_timeout_secs),
            max_lifetime: seconds(server.max_lifetime_secs),
        },
        lazy: server.lazy,
        schema_cache: Some(schema_cache(scope, name)),
    })
}

/// Where server `name`'s tool list is cached.
#[must_use]
pub fn schema_cache(scope: &ProfileScope, name: &str) -> PathBuf {
    scope
        .dir()
        .join("cache")
        .join("mcp-schemas")
        .join(format!("{name}.json"))
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
    /// `connected`, `idle`, `not connected`, `unavailable`, `disabled`,
    /// `error` or `skipped (safe mode)`.
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
    let skipped = skipped(scope);
    config
        .mcp
        .servers
        .iter()
        .map(|(name, server)| {
            let status = statuses.iter().find(|s| &s.name == name);
            let problem = skipped.iter().find(|(n, _)| n == name).map(|(_, why)| why);
            let (state, reason) = match status.map(|s| &s.state) {
                _ if !server.enabled => ("disabled".to_owned(), None),
                None if problem.is_some() => ("error".to_owned(), problem.cloned()),
                None => ("skipped (safe mode)".to_owned(), None),
                Some(State::Connected) => ("connected".to_owned(), None),
                Some(State::Idle) => ("idle".to_owned(), None),
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

#[cfg(test)]
mod tests;

//! A profile's MCP servers (spec: plugin-system, MCP client servers;
//! tool-system, MCP runtime safety; Registry is scoped per profile).
//!
//! The manager starts the servers a session needs, at most
//! `mcp.discovery_concurrency` at once, registers their tools in the
//! profile's tool registry and reports their state for `prometheus mcp
//! status`.

use std::sync::Arc;
use std::time::Duration;

use pmt_core::capability::CapabilitySet;
use pmt_tools::descriptor::Owner;
use pmt_tools::{ToolRegistry, Toolset};
use tokio::sync::Semaphore;

use crate::host::{ClientHost, NoHost};
use crate::naming;
use crate::server::{Launcher, Server, Settings, State};

/// One server to manage.
pub struct ServerEntry {
    /// The configured name.
    pub name: String,
    /// How it is used.
    pub settings: Settings,
    /// How it starts.
    pub launcher: Launcher,
}

/// One server as `mcp status` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerStatus {
    /// The configured name.
    pub name: String,
    /// `stdio` or `http`.
    pub transport: &'static str,
    /// Its state.
    pub state: State,
    /// Tools it listed at its last start (before filtering).
    pub tools: usize,
}

/// The servers of one profile.
#[derive(Debug)]
pub struct Manager {
    servers: Vec<Arc<Server>>,
    concurrency: usize,
    /// Distinguishes this set from every other (see [`Manager::generation`]).
    serial: u64,
}

impl Manager {
    /// Manages `entries`, starting at most `concurrency` at once, with no
    /// host (no session requests, no media store).
    #[must_use]
    pub fn new(entries: Vec<ServerEntry>, concurrency: usize) -> Self {
        Self::with_host(
            entries,
            concurrency,
            &(Arc::new(NoHost) as Arc<dyn ClientHost>),
        )
    }

    /// Manages `entries`, routing their session requests to `host`.
    #[must_use]
    pub fn with_host(
        entries: Vec<ServerEntry>,
        concurrency: usize,
        host: &Arc<dyn ClientHost>,
    ) -> Self {
        Self {
            servers: entries
                .into_iter()
                .map(|e| {
                    Arc::new(Server::new(
                        &e.name,
                        e.settings,
                        e.launcher,
                        Arc::clone(host),
                    ))
                })
                .collect(),
            concurrency: concurrency.max(1),
            serial: crate::server::next_serial(),
        }
    }

    /// Manages servers that already exist (a configuration change keeps
    /// the unchanged ones, connections included).
    #[must_use]
    pub fn from_servers(servers: Vec<Arc<Server>>, concurrency: usize) -> Self {
        Self {
            servers,
            concurrency: concurrency.max(1),
            serial: crate::server::next_serial(),
        }
    }

    /// The servers.
    #[must_use]
    pub fn servers(&self) -> &[Arc<Server>] {
        &self.servers
    }

    /// The server named `name`.
    #[must_use]
    pub fn server(&self, name: &str) -> Option<&Arc<Server>> {
        self.servers.iter().find(|s| s.name() == name)
    }

    /// Starts every server that is not connected, idle or backing off,
    /// at most `concurrency` at once, and waits for them. Returns each
    /// server's outcome.
    pub async fn discover(&self) -> Vec<(String, Result<(), String>)> {
        self.discover_within(None).await
    }

    /// [`Manager::discover`], waiting at most `wait`: servers still
    /// connecting then go on in the background (and join later sessions)
    /// and are reported as `still connecting`.
    pub async fn discover_within(
        &self,
        wait: Option<Duration>,
    ) -> Vec<(String, Result<(), String>)> {
        let servers = self.servers.clone();
        let permits = Arc::new(Semaphore::new(self.concurrency));
        crate::runtime::run(async move {
            let handles: Vec<_> = servers
                .into_iter()
                .map(|server| {
                    let permits = Arc::clone(&permits);
                    let name = server.name().to_owned();
                    let handle = tokio::spawn(async move {
                        let _permit = permits.acquire_owned().await.ok();
                        server.ensure().await
                    });
                    (name, handle)
                })
                .collect();
            let deadline = wait.map(|w| tokio::time::Instant::now() + w);
            let mut outcomes = Vec::new();
            for (name, handle) in handles {
                let outcome = match deadline {
                    Some(deadline) => match tokio::time::timeout_at(deadline, handle).await {
                        Ok(joined) => {
                            joined.unwrap_or_else(|e| Err(format!("the start failed: {e}")))
                        }
                        Err(_) => Err("still connecting".to_owned()),
                    },
                    None => handle
                        .await
                        .unwrap_or_else(|err| Err(format!("the start failed: {err}"))),
                };
                outcomes.push((name, outcome));
            }
            outcomes
        })
        .await
    }

    /// Registers each server's toolset `mcp-<server>` and its admitted
    /// tools (from its last listing) in `registry`; returns the toolsets
    /// that got tools. A name another owner holds is refused by the registry
    /// (and reported by `prometheus doctor`); the rest still register.
    pub fn register(&self, registry: &mut ToolRegistry) -> Vec<String> {
        let mut toolsets = Vec::new();
        for server in &self.servers {
            // The toolset exists even while the server has no tools, so its
            // name (and the server's bare name) stays a valid selection.
            let tools = crate::tool::registered(server);
            let owner = Owner::Mcp(server.name().to_owned());
            let name = naming::toolset(server.name());
            let toolset = Toolset::new(
                &name,
                &format!("Tools of MCP server '{}'", server.name()),
                &[],
            )
            .owned_by(owner.clone());
            if let Err(err) = registry.register_toolsets(&owner, vec![toolset]) {
                tracing::error!(server = %server.name(), "{}", err.message());
                continue;
            }
            let mut any = false;
            for tool in tools {
                any |= registry.register(tool, &CapabilitySet::default()).is_ok();
            }
            if any {
                toolsets.push(name);
            }
        }
        toolsets
    }

    /// Every server's state.
    #[must_use]
    pub fn statuses(&self) -> Vec<ServerStatus> {
        self.servers
            .iter()
            .map(|s| ServerStatus {
                name: s.name().to_owned(),
                transport: s.settings().transport,
                state: s.state(),
                tools: s.tools().len(),
            })
            .collect()
    }

    /// Changes whenever the server set or a server's tool list changes,
    /// so registries built from an older list are rebuilt.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.servers.iter().fold(self.serial, |acc, s| {
            acc.wrapping_mul(1_000_003)
                .wrapping_add(s.serial())
                .wrapping_mul(1_000_003)
                .wrapping_add(s.epoch())
        })
    }

    /// Stops every server.
    pub fn stop(&self) {
        for server in &self.servers {
            server.stop();
        }
    }
}

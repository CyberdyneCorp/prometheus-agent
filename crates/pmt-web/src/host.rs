//! What the dashboard needs from the daemon beyond the client protocol: a
//! served profile's configuration and store, its catalogs and redaction.
//! The daemon implements it; profile data is reached only through it, so
//! an unserved or unknown profile is never opened by the dashboard.

use std::sync::Arc;

use pmt_config::ProfileScope;
use pmt_gateway::HostFuture;
use pmt_store::Store;
use serde_json::Value;

/// A catalog of a profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Catalog {
    /// Its skills with their enabled state.
    Skills,
    /// Its toolsets with their enabled state.
    Toolsets,
    /// The provider/model pairs it is configured with.
    Models,
    /// Its plugins with their load state and problem.
    Plugins,
    /// Its configured MCP servers.
    Mcp,
    /// Its cron jobs with their state and next run.
    Cron,
    /// Its memory entries.
    Memory,
}

/// The daemon side of the dashboard.
pub trait DashboardHost: Send + Sync {
    /// The served profile's configuration now (`None`: not served).
    fn scope(&self, profile: &str) -> Option<ProfileScope>;

    /// The served profile's store (`None`: not served).
    fn store(&self, profile: &str) -> Option<Arc<Store>>;

    /// The profile the daemon was started with (machine-wide dashboard
    /// credentials live in its secrets store).
    fn launch_profile(&self) -> String;

    /// `text` with the profile's known secrets (every served profile's for
    /// `None`) and credential-shaped tokens redacted.
    fn redact(&self, profile: Option<&str>, text: &str) -> String;

    /// A catalog of the profile.
    fn catalog<'a>(&'a self, profile: &'a str, what: Catalog) -> HostFuture<'a, Value>;
}

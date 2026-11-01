//! Structured logs (spec: observability, Structured logs; cli, Log viewer).
//!
//! Every `tracing` record becomes one JSON line with the keys `ts`,
//! `level`, `component`, `profile`, `session_id`, `turn_id`, `trace_id`,
//! `msg` and `fields`. The context keys come from the enclosing spans (the
//! innermost span that sets one wins, then the event itself), so a record
//! emitted by a tool inside a turn span carries the turn's ids.
//!
//! Where a record goes ([`route`]):
//! - `gateway`, `update` and `migrate` records: `<home>/logs/<component>.log`;
//! - records of a profile: `<home>/profiles/<p>/logs/prometheus.log`;
//! - records without a profile (startup): `<home>/logs/prometheus.log`.
//!
//! Files are 0600, rotate at [`MAX_BYTES`] keeping [`KEEP`] files in all
//! (`prometheus.log`, `prometheus.log.1` ... `.4`), and every string is
//! redacted like trajectories before it is written.

mod layer;
mod rotate;
pub mod view;

pub use layer::{LogHandle, LogSetup, handle, install, layer};
pub use rotate::{Rotating, rotated};

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// A log file rotates when a write would take it past this size.
pub const MAX_BYTES: u64 = 10 * 1024 * 1024;

/// Files kept per log, the current one included.
pub const KEEP: usize = 5;

/// The log file of a profile, and of daemon-wide records under the home.
pub const FILE_NAME: &str = "prometheus.log";

/// The default component of a record that names none.
pub const DEFAULT_COMPONENT: &str = "agent";

/// Components whose records live under `<home>/logs/<component>.log`
/// whatever the profile.
pub const HOME_COMPONENTS: &[&str] = &["gateway", "update", "migrate"];

/// Every component `prometheus logs` accepts (spec: cli, Log viewer).
/// `errors` is not a producer: it selects WARN and above of every component.
pub const COMPONENTS: &[&str] = &[
    "agent",
    "errors",
    "gateway",
    "adapter",
    "cron",
    "kanban",
    "mcp",
    "plugins",
    "sandbox",
    "providers",
    "rpc",
    "update",
    "migrate",
    "evolve",
    "eval",
];

/// The span and event field names that set a record's context keys.
pub const CONTEXT_FIELDS: &[&str] = &["component", "profile", "session_id", "turn_id", "trace_id"];

/// One log record, as written and as read by `prometheus logs`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    /// RFC 3339 UTC with microseconds.
    pub ts: String,
    /// `error`, `warn`, `info`, `debug` or `trace`.
    pub level: String,
    /// The producing component (`agent` when none is named).
    pub component: String,
    /// The profile, `null` for daemon-wide records.
    pub profile: Option<String>,
    /// The session, inside one.
    pub session_id: Option<String>,
    /// The turn, inside one.
    pub turn_id: Option<String>,
    /// The trace, when tracing context exists.
    pub trace_id: Option<String>,
    /// The message.
    pub msg: String,
    /// Every other field of the event.
    pub fields: Map<String, Value>,
}

/// The file a record of `component` and `profile` is written to.
#[must_use]
pub fn route(home: &Path, component: &str, profile: Option<&str>) -> PathBuf {
    if HOME_COMPONENTS.contains(&component) {
        return home_dir(home).join(format!("{component}.log"));
    }
    match profile.and_then(|name| existing_profile_logs(home, name)) {
        Some(dir) => dir.join(FILE_NAME),
        None => home_dir(home).join(FILE_NAME),
    }
}

/// `<home>/logs`.
#[must_use]
pub fn home_dir(home: &Path) -> PathBuf {
    home.join("logs")
}

/// `<profile>/logs` when the profile exists. A record never creates a
/// profile directory, and a malformed profile field cannot leave the home.
fn existing_profile_logs(home: &Path, name: &str) -> Option<PathBuf> {
    let name = pmt_config::layout::ProfileName::parse(name).ok()?;
    let profile = home.join("profiles").join(name.to_string());
    profile.is_dir().then(|| profile.join("logs"))
}

/// Severity rank: lower is more severe (`error` = 0).
#[must_use]
pub fn severity(level: &str) -> Option<u8> {
    LEVELS
        .iter()
        .position(|l| *l == level)
        .and_then(|i| u8::try_from(i).ok())
}

/// Level names, most severe first.
pub const LEVELS: &[&str] = &["error", "warn", "info", "debug", "trace"];

#[cfg(test)]
mod tests {
    use super::{FILE_NAME, route, severity};

    #[test]
    fn records_are_routed_by_component_then_profile() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("profiles/work")).unwrap();
        let h = home.path();
        assert_eq!(route(h, "update", Some("work")), h.join("logs/update.log"));
        assert_eq!(
            route(h, "agent", Some("work")),
            h.join("profiles/work/logs").join(FILE_NAME)
        );
        assert_eq!(route(h, "agent", None), h.join("logs").join(FILE_NAME));
        assert_eq!(
            route(h, "agent", Some("ghost")),
            h.join("logs").join(FILE_NAME)
        );
        assert_eq!(
            route(h, "agent", Some("../x")),
            h.join("logs").join(FILE_NAME)
        );
    }

    #[test]
    fn severity_orders_levels() {
        assert!(severity("error") < severity("warn"));
        assert_eq!(severity("loud"), None);
    }
}

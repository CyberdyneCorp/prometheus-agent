//! Observation-only connections (spec: web-dashboard, Remote dashboard
//! users are observers by default; tui, observer = read-only): a
//! connection whose grant is `observer_only` may call only the methods
//! listed here. The list is an allowlist so that a writer added later is
//! refused until someone decides it is a read; session methods that drive
//! a session also check the attachment role themselves.

use pmt_core::error::{Error, codes};
use pmt_rpc::RpcError;

/// The methods that change nothing: the handshake, following sessions,
/// and read-only listings.
const READS: &[&str] = &[
    "rpc.hello",
    "ui.prefs.get",
    "rpc.ping",
    "session.attach",
    "session.detach",
    "session.list",
    "session.resolve",
    "session.tree",
    "session.siblings",
    "session.transcript",
    "session.events.since",
    "session.context_usage",
    "session.banner",
    "subagent.list",
    "commands.catalog",
    "complete.slash",
    "model.options",
    "config.get",
    "system.health",
    "system.metrics",
    "lsp.status",
    "journey.list",
    "memory.summary",
    "worktree.status",
    "worktree.list",
    "vault.list",
    "moa.presets.list",
    "local.status",
    "local.catalog",
    "local.jobs",
    "local.search",
    "cron.blueprints.list",
    "queue.list",
    "fs.list",
    "fs.read",
    "artifacts.list",
    "pty.attach",
    "pty.list",
];

/// `-32002 session.observer` when an observer-only connection calls a
/// method that is not a read.
pub(crate) fn check(observer_only: bool, method: &str) -> Result<(), RpcError> {
    if !observer_only || READS.contains(&method) {
        return Ok(());
    }
    if method.starts_with("pty.") {
        return Err(crate::pty::observer_refusal());
    }
    Err(RpcError::from_error(&Error::new(
        &codes::SESSION_OBSERVER,
        format!("observers have read-only access: {method} is refused"),
    )))
}

#[cfg(test)]
mod tests {
    use pmt_rpc::{Catalog, Kind};

    use super::{READS, check};

    #[test]
    fn every_listed_read_is_an_indexed_method() {
        let catalog = Catalog::builtin();
        for name in READS {
            assert!(
                catalog.entry(name).is_some_and(|e| e.kind == Kind::Method),
                "{name} is not an indexed method"
            );
        }
    }

    #[test]
    fn an_observer_may_read_but_not_write() {
        assert!(check(true, "vault.list").is_ok());
        let refused = check(true, "vault.add").unwrap_err();
        assert_eq!(refused.code, -32002);
        assert!(check(true, "session.create").is_err());
        assert!(check(false, "vault.add").is_ok(), "drivers are not limited");
    }
}

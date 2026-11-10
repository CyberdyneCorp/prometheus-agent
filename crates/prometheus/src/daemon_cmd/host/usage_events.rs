//! `usage.updated` (spec: observability, Live usage events): after every
//! model call of a session the daemon hosts, its attached clients get the
//! call's ledger row, the session's and the lineage's totals and the
//! budgets, all read from the ledger, so CLI, TUI, desktop and dashboard
//! show the same numbers.

use std::sync::{Arc, Weak};

use pmt_store::UsageEvent;

use super::DaemonHost;

/// Publishes `usage.updated` for every ledger row a hosted session
/// appends, from a task of its own (the turn does not wait for it).
pub(in crate::daemon_cmd) fn publish(host: &Arc<DaemonHost>, server: &Arc<pmt_gateway::Server>) {
    let (host, server) = (Arc::downgrade(host), Arc::downgrade(server));
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return;
    };
    pmt_obs::usage::live::on_recorded(move |profile, row| {
        let (host, server, profile, row) = (
            Weak::clone(&host),
            Weak::clone(&server),
            profile.to_owned(),
            row.clone(),
        );
        pmt_core::spawn::spawn_on(&runtime, async move {
            if let (Some(host), Some(server)) = (host.upgrade(), server.upgrade()) {
                announce(&host, &server, &profile, &row);
            }
        });
    });
}

fn announce(host: &DaemonHost, server: &pmt_gateway::Server, profile: &str, row: &UsageEvent) {
    let session = row.session_id.to_string();
    if server.hub().profile_of(&session).is_none() {
        return; // no client ever attached: nobody to tell
    }
    let Ok(entry) = host.profile(profile) else {
        return;
    };
    let scope = entry.scope();
    let zone = pmt_obs::time::profile_zone(&scope.config().cron).unwrap_or(jiff::tz::TimeZone::UTC);
    match pmt_obs::usage::live::payload(&entry.store, row, &scope.config().budgets, zone) {
        Ok(payload) => server.hub().publish(&session, "usage.updated", payload),
        Err(err) => tracing::debug!(session_id = session, "usage.updated not sent: {err}"),
    }
}

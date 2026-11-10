//! Components whose health is read rather than pushed (spec:
//! observability, Component health model): every [`EVERY`] the daemon
//! looks at each served profile's MCP servers, Python-bridge sidecars,
//! plugin host and provider circuits and reports their state; only a
//! change of state is logged and announced.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use pmt_obs::health::{self, State};

use super::DaemonHost;

/// Time between two samples.
const EVERY: Duration = Duration::from_secs(15);

/// Samples until the daemon stops.
pub(in crate::daemon_cmd) async fn sample(host: Arc<DaemonHost>) {
    loop {
        for (name, profile) in host.all_profiles() {
            let scope = profile.scope();
            let store = Arc::clone(&profile.store);
            let _ = tokio::task::spawn_blocking(move || sample_profile(&name, &scope, store)).await;
        }
        tokio::time::sleep(EVERY).await;
    }
}

fn sample_profile(profile: &str, scope: &pmt_config::ProfileScope, store: Arc<pmt_store::Store>) {
    let registry = health::global();
    for server in crate::mcp_host::manager(scope).servers() {
        let state = match server.state() {
            pmt_mcp::server::State::Connected | pmt_mcp::server::State::Idle => {
                Some((State::Ok, String::new()))
            }
            pmt_mcp::server::State::Unavailable(reason) => Some((State::Down, reason)),
            pmt_mcp::server::State::NotConnected => None,
        };
        if let Some((state, reason)) = state {
            registry.report(
                Some(profile),
                &format!("mcp:{}", server.name()),
                state,
                &reason,
            );
        }
    }
    for supervisor in crate::plugins_host::bridge::running(scope) {
        let status = supervisor.status();
        let state = match status.state {
            pmt_pybridge::supervisor::State::Crashed => State::Degraded,
            pmt_pybridge::supervisor::State::Failed => State::Down,
            _ => State::Ok,
        };
        let name = format!("python_bridge:{}", status.plugin);
        registry.report(Some(profile), &name, state, status.state.as_str());
    }
    plugin_host(profile, scope);
    provider_circuits(profile, scope, store);
}

/// `plugins`: degraded while an enabled plugin failed to load.
fn plugin_host(profile: &str, scope: &pmt_config::ProfileScope) {
    let loaded = crate::plugins_host::current(scope, EVERY).current();
    let failed: Vec<String> = loaded
        .statuses
        .iter()
        .filter(|s| s.state == pmt_plugins::load::State::Error)
        .map(|s| format!("{}: {}", s.id, s.problem.as_deref().unwrap_or("error")))
        .collect();
    let (state, reason) = if failed.is_empty() {
        (State::Ok, String::new())
    } else {
        (State::Degraded, failed.join("; "))
    };
    health::global().report(Some(profile), "plugins", state, &reason);
}

/// `provider:<id>`: degraded while its healthiest route scores below
/// `fallback.min_health` (fallback walks skip it).
fn provider_circuits(
    profile: &str,
    scope: &pmt_config::ProfileScope,
    store: Arc<pmt_store::Store>,
) {
    let min = scope.config().fallback.min_health;
    let Ok(records) = pmt_providers::health::StoreHealth::new(store, scope.config())
        .records(jiff::Timestamp::now())
    else {
        return;
    };
    let mut best: BTreeMap<String, f64> = BTreeMap::new();
    for record in records {
        let best_score = best.entry(record.provider).or_insert(f64::MIN);
        *best_score = best_score.max(record.score);
    }
    for (provider, score) in best {
        let (state, reason) = if score < min {
            (
                State::Degraded,
                format!("health score {score:.2} below fallback.min_health {min:.2}"),
            )
        } else {
            (State::Ok, format!("health score {score:.2}"))
        };
        health::global().report(
            Some(profile),
            &format!("provider:{provider}"),
            state,
            &reason,
        );
    }
}

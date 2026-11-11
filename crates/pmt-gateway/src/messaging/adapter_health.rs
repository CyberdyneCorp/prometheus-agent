//! An adapter's component health (spec: observability, Component health
//! model): `adapter:<platform>` of its profile is `ok` while connected,
//! `degraded` after [`DEGRADED_AFTER`] failed connections or polls in a
//! row (the last error as the reason) and `down` once it failed for good.

use std::sync::atomic::{AtomicU32, Ordering};

use pmt_obs::health::{self, State};

use super::adapter::{AdapterError, AdapterState};

/// Consecutive failures before an adapter is `degraded`.
pub(crate) const DEGRADED_AFTER: u32 = 3;

/// Reports `state` of `platform` (of `profile`) to the health registry;
/// `failures` counts the failures since the last connection.
pub(crate) fn report(
    profile: &str,
    platform: &str,
    state: AdapterState,
    error: Option<&AdapterError>,
    failures: &AtomicU32,
) {
    let name = format!("adapter:{platform}");
    let reason = || error.map_or_else(String::new, |e| format!("{}: {}", e.code, e.message));
    let registry = health::global();
    match state {
        AdapterState::Connected => {
            failures.store(0, Ordering::Relaxed);
            registry.report(Some(profile), &name, State::Ok, "connected");
        }
        AdapterState::Retrying => {
            let failed = failures.fetch_add(1, Ordering::Relaxed) + 1;
            if failed >= DEGRADED_AFTER {
                registry.report(Some(profile), &name, State::Degraded, &reason());
            }
        }
        AdapterState::Fatal => registry.report(Some(profile), &name, State::Down, &reason()),
        AdapterState::Disabled => registry.remove(Some(profile), &name),
        AdapterState::Connecting | AdapterState::Degraded => {}
    }
}

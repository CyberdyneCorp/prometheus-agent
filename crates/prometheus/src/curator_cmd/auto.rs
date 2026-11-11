//! Automatic curator passes (spec: skills, Curator configuration and
//! scheduling): the daemon's per-profile scheduler loop and the headless
//! `prometheus cron tick` ask [`check`] on every tick; a due pass runs on
//! its own thread (never on the async runtime), and the profile's
//! curator lock keeps it to one at a time.

use std::sync::Arc;

use jiff::Timestamp;
use pmt_config::ProfileScope;
use pmt_core::env::LaunchEnv;
use pmt_skills::curator::schedule::{self, Due};
use pmt_store::Store;

use super::Curator;

/// Whether a pass is due now (seeding the first observation).
#[must_use]
pub(crate) fn due(scope: &ProfileScope, store: &Store) -> bool {
    let last_activity = store.profile_last_activity().ok().flatten();
    match schedule::due(
        store,
        &scope.config().curator,
        Timestamp::now(),
        last_activity,
    ) {
        Ok(Due::Now) => true,
        Ok(Due::Seeded | Due::Later(_)) => false,
        Err(e) => {
            tracing::warn!(error = %e, "curator schedule check failed");
            false
        }
    }
}

/// Starts a pass on its own thread when one is due; the thread's handle.
#[must_use]
pub(crate) fn check(
    scope: &ProfileScope,
    store: &Arc<Store>,
    launch: &LaunchEnv,
) -> Option<std::thread::JoinHandle<()>> {
    if !due(scope, store) {
        return None;
    }
    let curator = Curator {
        scope: scope.clone(),
        store: Arc::clone(store),
        launch: launch.clone(),
    };
    let spawned = std::thread::Builder::new()
        .name("pmt-curator".to_owned())
        .spawn(move || match curator.run(false, false, true) {
            Ok(report) => tracing::info!(summary = %report.summary(), "automatic curator pass"),
            Err(e) => tracing::warn!(error = %e, "automatic curator pass failed"),
        });
    spawned.ok()
}

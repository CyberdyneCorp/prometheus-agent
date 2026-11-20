//! The daemon's pets methods and the rescan after each turn (spec: pets,
//! Achievements from session history; Achievement unlocks celebrate).

use std::sync::Arc;

use pmt_core::error::Error;
use pmt_gateway::Link;
use pmt_rpc::contracts::{AchievementUnlocked, AchievementUnlockedEvent};

use super::DaemonHost;

impl DaemonHost {
    /// One `pets.*` or `achievements.list` call for `profile`.
    pub(super) async fn pets_call(
        &self,
        profile: &str,
        request: pmt_gateway::pets::PetsCall,
    ) -> Result<serde_json::Value, Error> {
        let entry = self.profile(profile)?;
        let scope = entry.scope();
        crate::pets_host::rpc::handle(&scope, &entry.store, request).await
    }
}

/// After a turn: rescans the profile's history off the turn's path and
/// tells the profile's attached clients about each tier reached. Nothing
/// reaches the session or a model.
pub(super) fn rescan_after_turn(
    scope: &pmt_config::ProfileScope,
    store: &Arc<pmt_store::Store>,
    link: &Link,
) {
    let (scope, store, link) = (scope.clone(), Arc::clone(store), link.clone());
    drop(pmt_core::spawn::blocking(move || {
        let profile = scope.name().as_str().to_owned();
        for unlock in crate::pets_host::achievements::rescan(&scope, &store) {
            link.broadcast::<AchievementUnlockedEvent>(
                &profile,
                &AchievementUnlocked {
                    profile: profile.clone(),
                    id: unlock.id,
                    tier: unlock.tier,
                },
            );
        }
    }));
}

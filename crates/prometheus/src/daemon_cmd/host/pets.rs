//! The daemon's pets methods and the rescans that celebrate unlocks
//! (spec: pets, Achievements from session history; Achievement unlocks
//! celebrate).

use std::sync::Arc;

use pmt_core::error::Error;
use pmt_gateway::Link;
use pmt_pets::achievements::Unlock;
use pmt_rpc::contracts::Event as _;
use pmt_rpc::contracts::{AchievementUnlocked, AchievementUnlockedEvent};

use super::DaemonHost;

impl DaemonHost {
    /// One `pets.*` or `achievements.list` call for `profile`. Tiers a
    /// page-triggered scan reaches are broadcast like the post-turn
    /// rescan's: whichever scan records a tier first announces it.
    pub(super) async fn pets_call(
        &self,
        profile: &str,
        request: pmt_gateway::pets::PetsCall,
    ) -> Result<serde_json::Value, Error> {
        let entry = self.profile(profile)?;
        let scope = entry.scope();
        let server = self.server.get().cloned().unwrap_or_default();
        let name = profile.to_owned();
        let on_unlock = Box::new(move |unlocks: Vec<Unlock>| {
            if let Some(server) = server.upgrade() {
                for event in events(&name, unlocks) {
                    let data = serde_json::to_value(&event).unwrap_or_default();
                    server.hub().broadcast(
                        &name,
                        AchievementUnlockedEvent::NAME,
                        data,
                        pmt_gateway::hub::NO_CONN,
                    );
                }
            }
        });
        crate::pets_host::rpc::handle(&scope, &entry.store, request, on_unlock).await
    }

    /// Lets the host broadcast daemon-level events outside a turn.
    pub(in crate::daemon_cmd) fn set_server(&self, server: &Arc<pmt_gateway::Server>) {
        let _ = self.server.set(Arc::downgrade(server));
    }
}

/// The `achievement.unlocked` payloads for `profile`'s new tiers.
fn events(profile: &str, unlocks: Vec<Unlock>) -> impl Iterator<Item = AchievementUnlocked> {
    unlocks.into_iter().map(|unlock| AchievementUnlocked {
        profile: profile.to_owned(),
        id: unlock.id,
        tier: unlock.tier,
    })
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
        let unlocks = crate::pets_host::achievements::rescan(&scope, &store);
        for event in events(&profile, unlocks) {
            link.broadcast::<AchievementUnlockedEvent>(&profile, &event);
        }
    }));
}

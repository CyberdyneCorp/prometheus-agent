//! Achievements on this machine (spec: pets, Achievements from session
//! history; Achievement unlocks celebrate): the profile's achievements in
//! its time zone, and the rescan the daemon runs off the turn's path
//! after each finished turn, whose newly reached tiers become
//! `achievement.unlocked` events. Unlocks are never sent to a model and
//! never touch a session.

use pmt_config::ProfileScope;
use pmt_pets::achievements::{Profile, Unlock};

/// The achievements of `scope`'s profile (days and hours in its time
/// zone, `cron.timezone`).
#[must_use]
pub fn profile(scope: &ProfileScope) -> Profile {
    let zone = pmt_obs::time::profile_zone(&scope.config().cron).unwrap_or(jiff::tz::TimeZone::UTC);
    Profile::new(scope.dir(), zone)
}

/// Rescans after a turn; the tiers to celebrate. A profile never scanned
/// before records its past unlocks without a celebration.
#[must_use]
pub fn rescan(scope: &ProfileScope, store: &pmt_store::Store) -> Vec<Unlock> {
    match profile(scope).scan(store) {
        Ok(outcome) if !outcome.first => outcome.unlocked,
        Ok(_) => Vec::new(),
        Err(err) => {
            tracing::warn!("achievements scan failed: {}", err.message());
            Vec::new()
        }
    }
}

/// The line a line interface shows for an `achievement.unlocked` event.
#[must_use]
pub fn unlocked_line(data: &serde_json::Value) -> String {
    let id = data["id"].as_str().unwrap_or_default();
    let tier = data["tier"].as_str().unwrap_or_default();
    let name = pmt_pets::achievements::catalog::find(id).map_or(id, |a| a.name);
    format!("achievement unlocked: {name} ({tier})")
}

//! What changes while an MCP event subscription lives (spec: plugin-system,
//! MCP events): the server's grant (subscription id, `refreshBefore`, the
//! callback URL it was granted for) and the newest replay cursor, kept in
//! `<profile>/mcp-events.json` beside the subscription's webhook route in
//! `webhooks.toml`. The route is the subscription's fixed contract; this
//! file is written on every grant, refresh and cursor, so the route file
//! (which the webhook adapter re-reads when it changes) stays untouched.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The file, in the profile directory.
pub const FILE: &str = "mcp-events.json";

/// One route's subscription state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    /// The server's id for the subscription.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subscription_id: Option<String>,
    /// When the grant expires (RFC 3339; `None`: never).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_before: Option<String>,
    /// When the grant was received (Unix seconds; `None`: never granted).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub granted_at: Option<i64>,
    /// The callback URL the grant is for (half of the server's key).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// The newest replay cursor the server sent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

/// The file of the profile in `profile_dir`.
#[must_use]
pub fn path(profile_dir: &Path) -> PathBuf {
    profile_dir.join(FILE)
}

/// Every route's state (none without a readable file).
#[must_use]
pub fn load(profile_dir: &Path) -> BTreeMap<String, State> {
    std::fs::read(path(profile_dir))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Route `route`'s state (default when unknown).
#[must_use]
pub fn get(profile_dir: &Path, route: &str) -> State {
    load(profile_dir).remove(route).unwrap_or_default()
}

/// Applies `change` to route `route`'s state and writes the file
/// atomically, owner-only.
///
/// # Errors
///
/// The write failed.
pub fn update(
    profile_dir: &Path,
    route: &str,
    change: impl FnOnce(&mut State),
) -> std::io::Result<()> {
    let mut all = load(profile_dir);
    change(all.entry(route.to_owned()).or_default());
    write(profile_dir, &all)
}

/// Forgets route `route`.
///
/// # Errors
///
/// The write failed.
pub fn remove(profile_dir: &Path, route: &str) -> std::io::Result<()> {
    let mut all = load(profile_dir);
    if all.remove(route).is_none() {
        return Ok(());
    }
    write(profile_dir, &all)
}

fn write(profile_dir: &Path, all: &BTreeMap<String, State>) -> std::io::Result<()> {
    let text = serde_json::to_vec_pretty(all).unwrap_or_default();
    pmt_core::private_fs::write_atomic(&path(profile_dir), &text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_updates_and_removals_persist() {
        let dir =
            tempfile::tempdir_in(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target")).unwrap();
        assert_eq!(get(dir.path(), "r"), State::default());
        update(dir.path(), "r", |s| s.cursor = Some("c1".into())).unwrap();
        update(dir.path(), "r", |s| s.granted_at = Some(5)).unwrap();
        let state = get(dir.path(), "r");
        assert_eq!(
            (state.cursor.as_deref(), state.granted_at),
            (Some("c1"), Some(5))
        );
        remove(dir.path(), "r").unwrap();
        assert!(load(dir.path()).is_empty());
    }
}

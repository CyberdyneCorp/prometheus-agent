//! Machine-wide UI preferences (spec: desktop-app, Machine-wide UI
//! preferences): `ui.prefs.get` and `ui.prefs.patch`, stored once per
//! machine by the local daemon in `<home>/ui-prefs.json` and shared by
//! every desktop window and webapp tab, which watch for
//! `ui.prefs.changed`.
//!
//! Only display preferences are stored: credentials, grants, drafts and
//! per-window state (open tabs, pane sizes, scroll positions, zoom) are
//! refused by key. A file that does not parse is kept as
//! `ui-prefs.json.corrupt-<unix seconds>` and the store starts empty; the
//! open windows then add their values back with `only_missing`, which
//! never overwrites a key the store has.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};

use pmt_rpc::contracts::{self as c, AttachRole};
use pmt_rpc::{Catalog, RpcError};
use serde_json::Value;

use crate::router::Router;
use crate::server::{Call, Conn};

/// The store's file under the home.
pub const FILE: &str = "ui-prefs.json";

/// The display preferences a client may store.
pub const KEYS: &[&str] = &[
    "theme",
    "mode",
    "accent",
    "density",
    "layout_variant",
    "status_bar",
    "keybindings",
    "layouts",
    "interface_mode",
    "model_favourites",
    "dismissed_tips",
    "tours",
    // Opt-outs that hide desktop UI tools from the next session on, and
    // the first-run onboarding state (spec: desktop-app, Desktop UI
    // toolset contract; First-run onboarding chat).
    "message_reactions",
    "tours_enabled",
    "tips_enabled",
    "onboarding",
];

/// The largest value of one preference, serialized.
const MAX_VALUE: usize = 64 * 1024;

/// The store and the connections watching it.
#[derive(Default)]
pub struct UiPrefs {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    loaded: Option<c::UiPrefs>,
    watchers: Vec<Weak<Conn>>,
}

fn invalid(message: &str) -> RpcError {
    RpcError::invalid_params("/set", false, message)
}

/// Reads the store; a corrupt file is set aside.
fn load(path: &Path) -> c::UiPrefs {
    let empty = c::UiPrefs {
        prefs: c::PrefMap::new(),
        revision: 0,
    };
    let Ok(bytes) = std::fs::read(path) else {
        return empty;
    };
    if let Ok(prefs) = serde_json::from_slice::<c::UiPrefs>(&bytes) {
        return prefs;
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let aside = path.with_file_name(format!("{FILE}.corrupt-{stamp}"));
    if let Err(e) = std::fs::rename(path, &aside) {
        tracing::warn!("could not set the corrupt UI preferences aside: {e}");
    } else {
        tracing::warn!(kept = %aside.display(), "the UI preferences file was corrupt; rebuilding it from the open windows");
    }
    empty
}

fn save(path: &Path, prefs: &c::UiPrefs) -> Result<(), RpcError> {
    let text = serde_json::to_vec_pretty(prefs).map_err(|e| invalid(&e.to_string()))?;
    let next = path.with_file_name(format!("{FILE}.next"));
    std::fs::write(&next, text)
        .and_then(|()| std::fs::rename(&next, path))
        .map_err(|e| {
            RpcError::from_error(&pmt_core::error::Error::io(
                &pmt_core::error::codes::HOME_IO,
                path,
                &e,
            ))
        })
}

/// Checks every key and value of a patch.
fn check(set: &c::PrefMap) -> Result<(), RpcError> {
    for (key, value) in set {
        if !KEYS.contains(&key.as_str()) {
            return Err(invalid(&format!(
                "ui.prefs: '{key}' is not a display preference (credentials, drafts and per-window state are never stored here)"
            )));
        }
        if value.to_string().len() > MAX_VALUE {
            return Err(invalid(&format!("ui.prefs: '{key}' is larger than 64 KiB")));
        }
    }
    Ok(())
}

impl UiPrefs {
    fn current<'a>(state: &'a mut State, path: &Path) -> &'a mut c::UiPrefs {
        state.loaded.get_or_insert_with(|| load(path))
    }

    /// The preferences; `watch` adds `conn` to the watchers.
    fn get(&self, path: &Path, conn: &Arc<Conn>, watch: bool) -> c::UiPrefs {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if watch
            && !state
                .watchers
                .iter()
                .any(|w| w.upgrade().is_some_and(|c| c.id == conn.id))
        {
            state.watchers.push(Arc::downgrade(conn));
        }
        Self::current(&mut state, path).clone()
    }

    /// Applies a patch; answers the new preferences and who to tell.
    fn patch(
        &self,
        path: &Path,
        params: c::UiPrefsPatchParams,
    ) -> Result<(c::UiPrefs, Vec<Arc<Conn>>), RpcError> {
        check(&params.set)?;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut next = Self::current(&mut state, path).clone();
        let mut changed = false;
        for (key, value) in params.set {
            if params.only_missing && next.prefs.contains_key(&key) {
                continue;
            }
            let before = next.prefs.get(&key).cloned();
            if value.is_null() {
                next.prefs.remove(&key);
            } else {
                next.prefs.insert(key.clone(), value);
            }
            changed |= before != next.prefs.get(&key).cloned();
        }
        if !changed {
            return Ok((next, Vec::new()));
        }
        next.revision += 1;
        save(path, &next)?;
        state.loaded = Some(next.clone());
        state.watchers.retain(|w| w.strong_count() > 0);
        let watchers = state.watchers.iter().filter_map(Weak::upgrade).collect();
        Ok((next, watchers))
    }
}

/// Registers `ui.prefs.get` and `ui.prefs.patch`.
///
/// # Errors
///
/// `unregistered method name` for a method missing from `catalog`.
pub fn register(router: &mut Router, catalog: &Catalog) -> Result<(), String> {
    router
        .register::<c::UiPrefsGet, _, _>(catalog, get)?
        .register::<c::UiPrefsPatch, _, _>(catalog, patch)?;
    Ok(())
}

fn path(call: &Call) -> Result<PathBuf, RpcError> {
    call.server
        .home()
        .map(|home| home.join(FILE))
        .ok_or_else(|| invalid("this daemon keeps no UI preferences"))
}

async fn get(call: Call, params: c::UiPrefsGetParams) -> Result<c::UiPrefs, RpcError> {
    let path = path(&call)?;
    Ok(call.server.ui_prefs().get(&path, &call.conn, params.watch))
}

async fn patch(call: Call, params: c::UiPrefsPatchParams) -> Result<c::UiPrefs, RpcError> {
    let path = path(&call)?;
    let (prefs, watchers) = call.server.ui_prefs().patch(&path, params)?;
    if !watchers.is_empty() {
        let data = serde_json::to_value(&prefs).unwrap_or(Value::Null);
        let peers: Vec<_> = watchers
            .iter()
            .map(|w| w.peer(AttachRole::Observer))
            .collect();
        call.server.hub().notify(&peers, "ui.prefs.changed", data);
    }
    Ok(prefs)
}

#[cfg(test)]
mod tests {
    use super::{FILE, check, load};

    #[test]
    fn only_display_preferences_are_stored() {
        let mut set = serde_json::Map::new();
        set.insert("mode".into(), "dark".into());
        assert!(check(&set).is_ok());
        for refused in ["api_key", "draft", "open_tabs", "zoom"] {
            let mut bad = serde_json::Map::new();
            bad.insert(refused.into(), "x".into());
            assert!(check(&bad).is_err(), "{refused}");
        }
    }

    #[test]
    fn a_corrupt_file_is_set_aside_and_the_store_starts_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        std::fs::write(&path, "{not json").unwrap();
        let loaded = load(&path);
        assert!(loaded.prefs.is_empty());
        let names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names
                .iter()
                .any(|n| n.starts_with("ui-prefs.json.corrupt-")),
            "{names:?}"
        );
        assert!(!path.exists());
    }
}

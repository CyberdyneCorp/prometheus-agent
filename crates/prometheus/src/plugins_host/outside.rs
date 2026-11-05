//! Hooks fired outside any turn, waited for so they run before a command
//! exits: `on_skill_lifecycle` from `skills install|enable|disable`,
//! `on_session_delete` from `sessions delete|prune` and `on_plugin_loaded`
//! when a plugin set loads (spec: plugin-system, Typed hook registry).

use std::path::Path;
use std::sync::Arc;

use pmt_plugins::hooks::{Hooks, Payload};
use pmt_plugins::host::CallInfo;
use pmt_plugins::load::{Loaded, State};
use serde_json::{Map, Value, json};

/// Fires `hook` once per payload of `each` and waits for the callbacks:
/// on a thread of its own with its own runtime, so a caller inside or
/// outside an async runtime can wait without blocking that runtime's
/// tasks.
pub(super) fn fire_blocking(
    hooks: &Arc<Hooks>,
    hook: &'static str,
    each: Vec<Map<String, Value>>,
    cwd: &Path,
) {
    if each.is_empty() || !hooks.handles(hook) {
        return;
    }
    let call = CallInfo {
        session_id: None,
        surface: "cli".to_owned(),
        cwd: Some(cwd.display().to_string()),
    };
    let fire = async {
        for fields in each {
            hooks.fire(hook, &Payload::new(fields), &call).await;
        }
    };
    std::thread::scope(|scope| {
        scope.spawn(|| {
            match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime.block_on(fire),
                Err(err) => tracing::warn!("{hook} not fired: {err}"),
            }
        });
    });
}

/// `on_plugin_loaded` for every plugin of a set that just loaded.
pub(super) fn plugins_loaded(loaded: &Loaded, cwd: &Path) {
    let each = loaded
        .statuses
        .iter()
        .filter(|s| s.state == State::Loaded)
        .map(|s| {
            let kind = s.kind.map(pmt_plugins::manifest::Kind::as_str);
            super::object(json!({ "plugin_id": s.id, "version": s.version, "kind": kind }))
        })
        .collect();
    fire_blocking(&loaded.hooks, "on_plugin_loaded", each, cwd);
}

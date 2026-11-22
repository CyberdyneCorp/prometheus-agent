//! Host calls besides HTTP: secrets, profile state, logging.

use serde_json::Value;

use crate::bindings::prometheus::host::{log, secrets, state};

/// The granted secret `name`, if set.
#[must_use]
pub fn secret(name: &str) -> Option<String> {
    secrets::get(name).ok().filter(|v| !v.is_empty())
}

/// The value stored under `key` in the plugin's profile state.
#[must_use]
pub fn load(key: &str) -> Option<Value> {
    let bytes = state::get(key).ok()??;
    serde_json::from_slice(&bytes).ok()
}

/// Stores `value` under `key` in the plugin's profile state.
pub fn store(key: &str, value: &Value) {
    if let Err(e) = state::set(key, value.to_string().as_bytes()) {
        warn(&format!("cannot store {key}: {e:?}"));
    }
}

/// Logs a warning.
pub fn warn(message: &str) {
    log::log(log::Level::Warn, message);
}

/// Logs an informational line.
pub fn info(message: &str) {
    log::log(log::Level::Info, message);
}

/// Appends `line` to the plugin's file `path` (under its data
/// directory's `files/`), keeping its last `keep` lines.
pub fn append_line(path: &str, line: &str, keep: usize) {
    let old = state::read_file(path).unwrap_or_default();
    let text = String::from_utf8_lossy(&old);
    let mut lines: Vec<&str> = text.lines().collect();
    lines.push(line);
    let start = lines.len().saturating_sub(keep);
    let mut out = lines[start..].join("\n");
    out.push('\n');
    if let Err(e) = state::write_file(path, out.as_bytes()) {
        warn(&format!("cannot write {path}: {e:?}"));
    }
}

/// Keys of the plugin's profile state.
#[must_use]
pub fn keys() -> Vec<String> {
    state::keys().unwrap_or_default()
}

/// Removes `key` from the plugin's profile state.
pub fn remove(key: &str) {
    if let Err(e) = state::delete(key) {
        warn(&format!("cannot remove {key}: {e:?}"));
    }
}

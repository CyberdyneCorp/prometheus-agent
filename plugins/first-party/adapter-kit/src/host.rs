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

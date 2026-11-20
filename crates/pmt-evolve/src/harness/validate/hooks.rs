//! `hooks.toml` (spec: harness-evolution, Harness validation; Harness
//! execution confinement).
//!
//! The file is `[[hooks]]` entries with exactly the keys `event`,
//! `command`, `timeout_secs` (1-120, default 30) and `matcher`. Nothing
//! else is accepted, so a harness cannot declare network access, secrets,
//! capabilities or an approval mode: its hooks run in the session's
//! sandbox under the grants the session already has.

use pmt_config::harness::{HOOKS_FILE, Tree};
use serde::Deserialize;

use super::scripts::named_in;

/// Keys a hook entry may have.
pub const HOOK_KEYS: [&str; 4] = ["event", "command", "timeout_secs", "matcher"];
/// Longest hook timeout.
pub const MAX_TIMEOUT_SECS: u64 = 120;
/// Programs a hook command may start besides a harness script itself.
pub const INTERPRETERS: [&str; 4] = ["python3", "python", "sh", "bash"];

/// One `[[hooks]]` entry.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct HarnessHook {
    /// The hook event (the closed hook registry).
    pub event: String,
    /// The command: an interpreter and a harness script, or a script.
    pub command: String,
    /// Seconds one run may take.
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// A regular expression the tool name must match (tool events).
    #[serde(default)]
    pub matcher: Option<String>,
}

const fn default_timeout() -> u64 {
    30
}

#[derive(Deserialize)]
struct File {
    #[serde(default)]
    hooks: Vec<HarnessHook>,
}

/// The hooks of a valid tree (an invalid `hooks.toml` reads as none).
#[must_use]
pub fn hooks_of(tree: &Tree) -> Vec<HarnessHook> {
    tree.get(HOOKS_FILE)
        .and_then(|text| toml::from_str::<File>(text).ok())
        .map(|f| f.hooks)
        .unwrap_or_default()
}

/// Checks `hooks.toml`.
pub(super) fn check(tree: &Tree, errors: &mut Vec<String>) {
    let Some(text) = tree.get(HOOKS_FILE) else {
        return;
    };
    let table = match toml::from_str::<toml::Table>(text) {
        Ok(table) => table,
        Err(err) => {
            errors.push(format!("{HOOKS_FILE}: {}", err.message()));
            return;
        }
    };
    for key in table.keys().filter(|k| *k != "hooks") {
        errors.push(format!("unknown key '{key}' in {HOOKS_FILE}"));
    }
    let Some(entries) = table.get("hooks") else {
        return;
    };
    let Some(entries) = entries.as_array() else {
        errors.push(format!("{HOOKS_FILE}: hooks must be [[hooks]] entries"));
        return;
    };
    for entry in entries {
        match entry.as_table() {
            Some(entry) => check_entry(tree, entry, errors),
            None => errors.push(format!("{HOOKS_FILE}: hooks must be [[hooks]] entries")),
        }
    }
}

fn check_entry(tree: &Tree, entry: &toml::Table, errors: &mut Vec<String>) {
    for key in entry.keys().filter(|k| !HOOK_KEYS.contains(&k.as_str())) {
        errors.push(format!("unknown key '{key}' in {HOOKS_FILE}"));
    }
    match entry.get("event").and_then(toml::Value::as_str) {
        Some(event) if pmt_core::hooks::HOOK_NAMES.contains(&event) => {}
        Some(event) => errors.push(format!("unknown hook event '{event}'")),
        None => errors.push(format!("{HOOKS_FILE}: a hook needs an event")),
    }
    match entry.get("timeout_secs") {
        None => {}
        Some(toml::Value::Integer(n)) if (1..=120).contains(n) => {}
        Some(_) => errors.push(format!(
            "{HOOKS_FILE}: timeout_secs must be from 1 to {MAX_TIMEOUT_SECS}"
        )),
    }
    if let Some(matcher) = entry.get("matcher") {
        let valid = matcher
            .as_str()
            .is_some_and(|m| regex::Regex::new(m).is_ok());
        if !valid {
            errors.push(format!(
                "{HOOKS_FILE}: matcher must be a regular expression"
            ));
        }
    }
    match entry.get("command").and_then(toml::Value::as_str) {
        Some(command) => check_command(tree, command, errors),
        None => errors.push(format!("{HOOKS_FILE}: a hook needs a command")),
    }
}

/// A command starts an interpreter or a harness script and names only
/// harness scripts.
fn check_command(tree: &Tree, command: &str, errors: &mut Vec<String>) {
    let Some(words) = shlex::split(command).filter(|w| !w.is_empty()) else {
        errors.push(format!("{HOOKS_FILE}: command '{command}' does not parse"));
        return;
    };
    let scripts: Vec<&str> = named_in(&words).collect();
    for script in &scripts {
        if !(tree.files.contains_key(*script) && crate::harness::surface::is_script(script)) {
            errors.push(format!(
                "{HOOKS_FILE}: command '{command}' names {script}, which is not a harness script"
            ));
        }
    }
    let program = words[0].trim_start_matches("./");
    if scripts.is_empty() || !(INTERPRETERS.contains(&program) || scripts.contains(&program)) {
        errors.push(format!(
            "{HOOKS_FILE}: command '{command}' must run a harness script (directly or with {})",
            INTERPRETERS.join(", ")
        ));
    }
}

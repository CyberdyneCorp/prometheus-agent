//! What a reloaded `config.toml` changes, by apply class (spec:
//! configuration-and-profiles, Live reload with apply classes): `live` keys
//! take effect at once, `next_session` keys for sessions created
//! afterwards, `restart` keys only after the process restarts. A long-
//! running process diffs the effective configuration it serves against the
//! freshly validated one and applies each class accordingly.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::catalog::{self, ApplyClass};
use crate::schema::Config;

/// One changed key and when it takes effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// The dotted key (a map entry's key names the entry, for example
    /// `providers.openrouter.base_url`).
    pub key: String,
    /// Its apply class.
    pub apply: ApplyClass,
    /// The value before the change (`None` when it was unset).
    pub old: Option<Value>,
}

/// Every key whose effective value differs between `old` and `new`, in
/// key order.
#[must_use]
pub fn changes(old: &Config, new: &Config) -> Vec<Change> {
    let mut before = BTreeMap::new();
    let mut after = BTreeMap::new();
    flatten(
        "",
        &serde_json::to_value(old).unwrap_or_default(),
        &mut before,
    );
    flatten(
        "",
        &serde_json::to_value(new).unwrap_or_default(),
        &mut after,
    );
    let mut keys: Vec<&String> = before.keys().chain(after.keys()).collect();
    keys.sort();
    keys.dedup();
    keys.into_iter()
        .filter(|key| before.get(*key) != after.get(*key))
        .map(|key| Change {
            key: key.clone(),
            apply: apply_class(key),
            old: before.get(key).cloned(),
        })
        .collect()
}

/// The apply class of `key`, or of its nearest catalogued parent (a map
/// entry nobody described takes its section's class).
fn apply_class(key: &str) -> ApplyClass {
    let mut key = key;
    loop {
        if let Some(info) = catalog::lookup(key) {
            return info.apply;
        }
        match key.rsplit_once('.') {
            Some((parent, _)) => key = parent,
            None => return ApplyClass::NextSession,
        }
    }
}

/// Dotted keys of every scalar or array value under `value`; `null`
/// (unset) values are left out.
fn flatten(prefix: &str, value: &Value, out: &mut BTreeMap<String, Value>) {
    match value {
        Value::Object(map) => {
            for (name, child) in map {
                let key = if prefix.is_empty() {
                    name.clone()
                } else {
                    format!("{prefix}.{name}")
                };
                flatten(&key, child, out);
            }
        }
        Value::Null => {}
        other => {
            out.insert(prefix.to_owned(), other.clone());
        }
    }
}

/// The `--set` arguments that keep `restart` keys at the values the
/// process started with, so a reload applies everything else; keys that
/// were unset before cannot be pinned this way and are returned apart.
#[must_use]
pub fn pin_restart_keys(changes: &[Change]) -> (Vec<String>, Vec<String>) {
    let mut pins = Vec::new();
    let mut unpinnable = Vec::new();
    for change in changes.iter().filter(|c| c.apply == ApplyClass::Restart) {
        match change.old.as_ref().and_then(toml_literal) {
            Some(literal) => pins.push(format!("{}={literal}", change.key)),
            None => unpinnable.push(change.key.clone()),
        }
    }
    (pins, unpinnable)
}

fn toml_literal(value: &Value) -> Option<String> {
    let value = toml::Value::try_from(value).ok()?;
    Some(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::{changes, pin_restart_keys};
    use crate::catalog::ApplyClass;
    use crate::schema::Config;

    #[test]
    fn changed_keys_carry_their_apply_class() {
        let old = Config::default();
        let mut new = Config::default();
        new.prompt.load_identity = false;
        new.approvals.timeout_secs = 5;
        new.logging.level = crate::schema::LogLevel::Debug;
        let found: Vec<(String, ApplyClass)> = changes(&old, &new)
            .into_iter()
            .map(|c| (c.key, c.apply))
            .collect();
        assert_eq!(
            found,
            [
                ("approvals.timeout_secs".to_owned(), ApplyClass::Live),
                ("logging.level".to_owned(), ApplyClass::Restart),
                ("prompt.load_identity".to_owned(), ApplyClass::NextSession),
            ]
        );
        assert!(changes(&old, &old).is_empty());
    }

    #[test]
    fn restart_keys_are_pinned_to_their_old_values() {
        let old = Config::default();
        let mut new = Config::default();
        new.logging.level = crate::schema::LogLevel::Debug;
        let (pins, unpinnable) = pin_restart_keys(&changes(&old, &new));
        assert_eq!(pins, ["logging.level=\"info\""]);
        assert!(unpinnable.is_empty());
    }
}

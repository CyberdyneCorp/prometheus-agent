//! The remote plane's wire shapes (spec: configuration-and-profiles,
//! Remote configuration backend) and the two pure transformations the
//! client needs: layering a fetched document into one `config.toml` text,
//! and turning a local edit back into the changed keys only.
//!
//! A fetch answers
//! `{"version": "<etag>", "layers": [{"level": "global", "values": {..},
//! "locked": ["approvals.mode"]}, ..]}`; a write sends
//! `{"set": {"<dotted.key>": <value>}, "unset": ["<dotted.key>"]}` with
//! `If-Match: <version>`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The plane's levels, lowest precedence first: a later level's value
/// wins over an earlier one's.
pub const LEVELS: [&str; 6] = ["global", "tenant", "group", "owner", "agent", "profile"];

/// The level every write of this client lands on.
pub const WRITE_LEVEL: &str = "profile";

/// What a fetch returns.
#[derive(Debug, Clone, Deserialize)]
pub struct Fetched {
    /// The document version (sent back as `If-Match` on writes).
    pub version: String,
    /// The layers, in any order.
    #[serde(default)]
    pub layers: Vec<Layer>,
}

/// One level's values and locks.
#[derive(Debug, Clone, Deserialize)]
pub struct Layer {
    /// One of [`LEVELS`].
    pub level: String,
    /// Nested values, shaped like `config.toml`.
    #[serde(default)]
    pub values: Map<String, Value>,
    /// Dotted keys this level locks for every level above it.
    #[serde(default)]
    pub locked: Vec<String>,
}

/// A write: the changed keys only.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Patch {
    /// Keys set, with their new values.
    pub set: BTreeMap<String, Value>,
    /// Keys removed (their lower levels or defaults apply again).
    pub unset: Vec<String>,
}

impl Patch {
    /// Whether nothing changed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.set.is_empty() && self.unset.is_empty()
    }

    /// Every key the patch touches.
    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.set.keys().chain(&self.unset)
    }
}

/// The layered document: the merged values as `config.toml` text and the
/// level that locks each locked key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layered {
    /// The merged values as TOML, with the current `schema_version`.
    pub text: String,
    /// Locked key to the lowest level locking it.
    pub locks: BTreeMap<String, String>,
}

/// Merges `fetched`'s layers in [`LEVELS`] order.
///
/// # Errors
///
/// A message for an unknown level, a value TOML cannot hold, or a
/// `schema_version` other than this build's.
pub fn layer(fetched: &Fetched) -> Result<Layered, String> {
    let mut layers: Vec<&Layer> = fetched.layers.iter().collect();
    for layer in &layers {
        if !LEVELS.contains(&layer.level.as_str()) {
            return Err(format!(
                "unknown level '{}' (levels: {})",
                layer.level,
                LEVELS.join(", ")
            ));
        }
    }
    layers.sort_by_key(|l| LEVELS.iter().position(|level| *level == l.level));
    let mut merged = Map::new();
    let mut locks = BTreeMap::new();
    for layer in layers {
        merge(&mut merged, &layer.values);
        for key in &layer.locked {
            locks
                .entry(key.clone())
                .or_insert_with(|| layer.level.clone());
        }
    }
    match merged.remove("schema_version") {
        None => {}
        Some(Value::Number(n)) if n.as_u64() == Some(u64::from(crate::SCHEMA_VERSION)) => {}
        Some(other) => {
            return Err(format!(
                "schema_version {other} is not this build's {}",
                crate::SCHEMA_VERSION
            ));
        }
    }
    strip_nulls(&mut merged);
    let body = toml::to_string(&Value::Object(merged))
        .map_err(|e| format!("the values are not representable as TOML: {e}"))?;
    Ok(Layered {
        text: format!("schema_version = {}\n{body}", crate::SCHEMA_VERSION),
        locks,
    })
}

/// Deep-merges `over` into `base`: tables merge, everything else replaces.
fn merge(base: &mut Map<String, Value>, over: &Map<String, Value>) {
    for (key, value) in over {
        match (base.get_mut(key), value) {
            (Some(Value::Object(inner)), Value::Object(more)) => merge(inner, more),
            _ => {
                base.insert(key.clone(), value.clone());
            }
        }
    }
}

/// TOML has no null: a null removes the key (an upper level clearing a
/// lower level's value).
fn strip_nulls(map: &mut Map<String, Value>) {
    map.retain(|_, v| !v.is_null());
    for value in map.values_mut() {
        if let Value::Object(inner) = value {
            strip_nulls(inner);
        }
    }
}

/// The keys `new` changes relative to `old` (both `config.toml` texts):
/// leaves (scalars and arrays) set to a different value, and leaves
/// removed. `schema_version` is never part of a patch.
///
/// # Errors
///
/// A message when either text is not TOML.
pub fn diff(old: &str, new: &str) -> Result<Patch, String> {
    let leaves = |text: &str| -> Result<BTreeMap<String, Value>, String> {
        let table: toml::Table = toml::from_str(text).map_err(|e| e.message().to_owned())?;
        let json = serde_json::to_value(table).map_err(|e| e.to_string())?;
        let mut out = BTreeMap::new();
        flatten("", &json, &mut out);
        out.remove("schema_version");
        Ok(out)
    };
    let (before, after) = (leaves(old)?, leaves(new)?);
    let mut patch = Patch::default();
    for (key, value) in &after {
        if before.get(key) != Some(value) {
            patch.set.insert(key.clone(), value.clone());
        }
    }
    patch.unset = before
        .keys()
        .filter(|key| !after.contains_key(*key))
        .cloned()
        .collect();
    Ok(patch)
}

fn flatten(prefix: &str, value: &Value, out: &mut BTreeMap<String, Value>) {
    match value {
        Value::Object(map) if !map.is_empty() || prefix.is_empty() => {
            for (name, child) in map {
                let key = if prefix.is_empty() {
                    name.clone()
                } else {
                    format!("{prefix}.{name}")
                };
                flatten(&key, child, out);
            }
        }
        other => {
            out.insert(prefix.to_owned(), other.clone());
        }
    }
}

/// The level locking `key` (the key itself or a parent table of it).
#[must_use]
pub fn locked_by<'a>(locks: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    locks.iter().find_map(|(locked, level)| {
        let covers = key == locked
            || key.starts_with(locked.as_str()) && key[locked.len()..].starts_with('.');
        covers.then_some(level.as_str())
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{Fetched, diff, layer, locked_by};

    fn fetched(value: serde_json::Value) -> Fetched {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn upper_levels_win_and_the_lowest_lock_names_its_level() {
        let doc = fetched(json!({
            "version": "7",
            "layers": [
                {"level": "profile", "values": {"model": {"default": "p"}}},
                {"level": "global", "values": {"model": {"default": "g", "provider": "openrouter"}},
                 "locked": ["approvals.mode"]},
                {"level": "tenant", "values": {"approvals": {"mode": "manual"}}, "locked": ["approvals.mode"]}
            ]
        }));
        let layered = layer(&doc).unwrap();
        let table: toml::Table = toml::from_str(&layered.text).unwrap();
        assert_eq!(table["model"]["default"].as_str(), Some("p"));
        assert_eq!(table["model"]["provider"].as_str(), Some("openrouter"));
        assert_eq!(
            table["schema_version"].as_integer(),
            Some(i64::from(crate::SCHEMA_VERSION))
        );
        assert_eq!(locked_by(&layered.locks, "approvals.mode"), Some("global"));
        assert_eq!(locked_by(&layered.locks, "approvals"), None);
        assert_eq!(locked_by(&layered.locks, "approvals.modes"), None);
    }

    #[test]
    fn an_unknown_level_or_an_old_schema_is_refused() {
        let unknown = fetched(json!({"version": "1", "layers": [{"level": "team", "values": {}}]}));
        assert!(
            layer(&unknown)
                .unwrap_err()
                .contains("unknown level 'team'")
        );
        let old = fetched(
            json!({"version": "1", "layers": [{"level": "global", "values": {"schema_version": 999}}]}),
        );
        assert!(layer(&old).unwrap_err().contains("schema_version"));
    }

    #[test]
    fn a_diff_holds_only_the_changed_leaves() {
        let old =
            "schema_version = 1\n[model]\ndefault = \"a\"\nprovider = \"x\"\n[ui]\nbell = true\n";
        let new = "schema_version = 1\n[model]\ndefault = \"b\"\nprovider = \"x\"\n";
        let patch = diff(old, new).unwrap();
        assert_eq!(patch.set.keys().collect::<Vec<_>>(), ["model.default"]);
        assert_eq!(patch.unset, ["ui.bell"]);
        assert!(diff(old, old).unwrap().is_empty());
    }
}

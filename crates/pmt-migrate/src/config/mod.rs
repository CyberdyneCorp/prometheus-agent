//! The `config` component (spec: hermes-migration, Config translation
//! from user-set values only; Supported Hermes config versions; Ambiguous
//! YAML values resolve fail-closed; Config references to environment
//! values; Typed names validated against Prometheus registries).
//!
//! Only keys present in the profile's own `config.yaml` are read (never
//! Hermes defaults, never `/etc/hermes`). Each gets one entry; values that
//! map are written in one validated `config.toml` change.

pub mod refs;
pub mod table;
mod value;

use std::path::Path;

use pmt_core::error::{Error, codes};
use serde_json::Value;

use crate::component::Component;
use crate::data;
use crate::plan::{Dest, Existing, Source, Step};
use crate::report::{Disposition, Entry};
use crate::writes::Writes;

pub use value::{json_to_toml, show};

/// The oldest Hermes `_config_version` the migrator reads.
pub const MIN_VERSION: u64 = 12;

/// The planned config component.
pub struct ConfigStep {
    entries: Vec<Entry>,
    writes: Writes,
}

impl Step for ConfigStep {
    fn component(&self) -> Component {
        Component::Config
    }

    fn entries(&mut self) -> &mut Vec<Entry> {
        &mut self.entries
    }

    fn predict(&mut self, existing: &Existing<'_>) {
        self.writes.predict(existing, &mut self.entries);
    }

    fn apply(&mut self, dest: &Dest<'_>) -> Result<(), Error> {
        self.writes.apply(dest, &mut self.entries)
    }
}

/// Plans the config component; `warnings` receives the newer-version
/// notice.
///
/// # Errors
///
/// `migrate.component` for a config older than version 12.
pub fn plan(src: &Source<'_>, warnings: &mut Vec<String>) -> Result<ConfigStep, Error> {
    let mut step = ConfigStep {
        entries: Vec::new(),
        writes: Writes::default(),
    };
    step.entries.extend(managed_layer(Path::new(MANAGED_DIR)));
    if src
        .config
        .value
        .as_object()
        .is_none_or(serde_json::Map::is_empty)
    {
        return Ok(step);
    }
    check_version(src, warnings)?;
    let mut leaves = Vec::new();
    flatten(&src.config.value, "", &mut leaves);
    for (key, value) in leaves {
        if key == "_config_version" {
            step.entries.push(
                Entry::new(Component::Config, key, Disposition::Skipped)
                    .because("Hermes bookkeeping"),
            );
            continue;
        }
        if table::owner(&key).is_some() {
            continue;
        }
        let coerced = src.config.coerced.contains(&key);
        let (entry, write) = value::plan_key(src, &key, value, coerced);
        let index = step.entries.len();
        step.entries.push(entry);
        if let Some((target, value)) = write {
            step.writes.key(target, value, index);
        }
    }
    Ok(step)
}

/// The Hermes administrator layer (an overlay on every profile).
pub const MANAGED_DIR: &str = "/etc/hermes";

/// The managed layer's report entry: it is policy of this machine's
/// administrator, never carried into a profile.
fn managed_layer(dir: &Path) -> Option<Entry> {
    let file = dir.join("config.yaml");
    file.is_file().then(|| {
        Entry::new(Component::Config, file.display().to_string(), Disposition::Skipped).because(
            "the Hermes managed layer is the administrator's policy; it is not migrated (Prometheus has its own managed layer)",
        )
    })
}

fn check_version(src: &Source<'_>, warnings: &mut Vec<String>) -> Result<(), Error> {
    let known = u64::from(data::known_config_version());
    let Some(version) = src.get("_config_version").and_then(Value::as_u64) else {
        warnings.push(format!(
            "Hermes config.yaml has no _config_version; read as version {known}"
        ));
        return Ok(());
    };
    if version < MIN_VERSION {
        return Err(Error::new(
            &codes::MIGRATE_COMPONENT,
            "Hermes config predates version 12; run hermes update first",
        ));
    }
    if version > known {
        warnings.push(format!(
            "Hermes config version {version} is newer than this migrator knows ({known}); unknown keys will be reported"
        ));
    }
    Ok(())
}

/// Every leaf of `value` as `(dotted key, value)`: scalars, lists and
/// empty mappings.
pub fn flatten<'a>(value: &'a Value, path: &str, out: &mut Vec<(String, &'a Value)>) {
    match value.as_object() {
        Some(map) if !map.is_empty() => {
            for (k, v) in map {
                let key = if path.is_empty() {
                    k.clone()
                } else {
                    format!("{path}.{k}")
                };
                flatten(v, &key, out);
            }
        }
        _ => out.push((path.to_owned(), value)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_managed_layer_is_reported_not_read() {
        let dir = tempfile::tempdir().unwrap();
        assert!(managed_layer(dir.path()).is_none());
        std::fs::write(
            dir.path().join("config.yaml"),
            "approvals:\n  mode: manual\n",
        )
        .unwrap();
        let entry = managed_layer(dir.path()).unwrap();
        assert_eq!(entry.disposition, Disposition::Skipped);
        assert!(entry.reason.contains("managed layer"));
    }
}

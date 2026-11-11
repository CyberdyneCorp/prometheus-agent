//! Config keys and secrets a component writes, applied as one unit.
//!
//! A destination that already holds another value keeps it and the item
//! becomes a `conflict` (spec: hermes-migration, Idempotent incremental
//! re-run) unless `--overwrite` is given. The config keys go in one
//! validated write ([`pmt_config::ConfigFile::set_values`]), so a component
//! never leaves a half-written `config.toml`.

use std::path::Path;

use pmt_config::ProfileScope;
use pmt_core::error::Error;
use pmt_core::secret::SecretString;
use toml_edit::{DocumentMut, Item, Value};

use crate::report::{Disposition, Entry};

/// A config key to write, with the report entry it belongs to.
#[derive(Debug, Clone)]
pub struct KeyWrite {
    /// The dotted Prometheus key.
    pub key: String,
    /// The value.
    pub value: Value,
    /// Index of its entry in the component's entries.
    pub entry: usize,
}

/// A secret to store.
#[derive(Debug, Clone)]
pub struct SecretWrite {
    /// The secret name.
    pub name: String,
    /// The value.
    pub value: SecretString,
    /// Index of its entry in the component's entries.
    pub entry: usize,
}

/// What one component writes.
#[derive(Debug, Clone, Default)]
pub struct Writes {
    /// Config keys.
    pub config: Vec<KeyWrite>,
    /// Secrets.
    pub secrets: Vec<SecretWrite>,
}

impl Writes {
    /// Adds a key for entry `entry`.
    pub fn key(&mut self, key: impl Into<String>, value: impl Into<Value>, entry: usize) {
        self.config.push(KeyWrite {
            key: key.into(),
            value: value.into(),
            entry,
        });
    }

    /// Adds a secret for entry `entry`.
    pub fn secret(&mut self, name: impl Into<String>, value: &str, entry: usize) {
        self.secrets.push(SecretWrite {
            name: name.into(),
            value: SecretString::new(value.to_owned()),
            entry,
        });
    }

    /// Writes everything into `scope`'s profile, turning entries whose
    /// destination differs into conflicts unless `overwrite`.
    ///
    /// # Errors
    ///
    /// The config file's or the secrets store's errors; nothing of the
    /// config is written then.
    pub fn apply(
        &self,
        scope: &ProfileScope,
        entries: &mut [Entry],
        overwrite: bool,
    ) -> Result<(), Error> {
        let store = scope.secrets().store();
        let mut current = std::collections::BTreeMap::new();
        for s in &self.secrets {
            if let Some(value) = store.get(&s.name)? {
                current.insert(s.name.clone(), value.expose().to_owned());
            }
        }
        let file = scope.profile().config_file();
        let text = file.raw()?.unwrap_or_default();
        let (keys, secrets) = self.pending(&text, &current, entries, overwrite);
        if !keys.is_empty() {
            file.set_values(&keys)?;
        }
        for s in secrets {
            store.set(&s.name, &s.value)?;
        }
        Ok(())
    }

    /// What a dry run predicts: the conflicts with an existing profile at
    /// `profile_dir`, read without writing (`config.toml`, and
    /// `secrets.toml` of the file backend).
    pub fn predict(&self, profile_dir: &Path, entries: &mut [Entry], overwrite: bool) {
        let text = std::fs::read_to_string(profile_dir.join("config.toml")).unwrap_or_default();
        let secrets: std::collections::BTreeMap<String, String> =
            std::fs::read_to_string(profile_dir.join("secrets.toml"))
                .ok()
                .and_then(|t| toml::from_str::<toml::Table>(&t).ok())
                .map(|t| {
                    t.into_iter()
                        .filter_map(|(k, v)| v.as_str().map(|v| (k, v.to_owned())))
                        .collect()
                })
                .unwrap_or_default();
        let _ = self.pending(&text, &secrets, entries, overwrite);
    }

    /// The keys and secrets to write given the destination's `config`
    /// text and `secrets`; the others become conflicts (unless
    /// `overwrite`) or need nothing (equal values).
    fn pending(
        &self,
        config: &str,
        secrets: &std::collections::BTreeMap<String, String>,
        entries: &mut [Entry],
        overwrite: bool,
    ) -> (Vec<(String, Value)>, Vec<&SecretWrite>) {
        let mut writes = Vec::new();
        for s in &self.secrets {
            match secrets.get(&s.name) {
                Some(old) if old == s.value.expose() => {}
                Some(_) if !overwrite => {
                    conflict(
                        &mut entries[s.entry],
                        "the secrets store holds another value",
                    );
                }
                _ => writes.push(s),
            }
        }
        let doc: DocumentMut = config.parse().unwrap_or_default();
        let mut keys = Vec::new();
        for k in &self.config {
            match lookup(&doc, &k.key) {
                Some(old) if same(old, &k.value) => {}
                Some(_) if !overwrite => conflict(
                    &mut entries[k.entry],
                    &format!("config.toml already sets {}", k.key),
                ),
                _ => keys.push((k.key.clone(), k.value.clone())),
            }
        }
        (keys, writes)
    }
}

fn conflict(entry: &mut Entry, why: &str) {
    entry.disposition = Disposition::Conflict;
    entry.reason = format!("{why}; kept (use --overwrite to replace it)");
}

/// The item at dotted `key` of `doc` (a value or a table).
#[must_use]
pub fn lookup<'a>(doc: &'a DocumentMut, key: &str) -> Option<&'a Item> {
    let mut item: &Item = doc.as_item();
    for segment in key.split('.') {
        item = item.as_table_like()?.get(segment)?;
    }
    Some(item)
}

/// Whether the existing `item` holds `value` (a table never compares
/// equal: it was written by hand).
fn same(item: &Item, value: &Value) -> bool {
    item.as_value()
        .is_some_and(|old| normalize(old) == normalize(value))
}

/// `v` as a plain TOML value, so formatting differences compare equal.
fn normalize(v: &Value) -> Option<toml::Value> {
    let mut table: toml::Table = toml::from_str(&format!("v = {v}")).ok()?;
    table.remove("v")
}

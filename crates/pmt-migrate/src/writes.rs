//! Config keys and secrets a component writes, applied as one unit.
//!
//! Each item is decided against the migration ledger ([`crate::ledger`]):
//! a destination edited since the previous run keeps its value and the
//! item becomes a `conflict` (spec: hermes-migration, Idempotent
//! incremental re-run) unless `--overwrite` is given. The config keys go in one
//! validated write ([`pmt_config::ConfigFile::set_values`]), so a component
//! never leaves a half-written `config.toml`.

use std::collections::BTreeMap;

use pmt_core::error::Error;
use pmt_core::secret::SecretString;
use toml_edit::Value;

use crate::ledger::{Decision, Ledger, UNCHANGED, decide, digest};
use crate::plan::{Dest, Existing};
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

    /// Writes everything into the destination profile. Against the
    /// ledger: an item whose source did not change since the last run is
    /// left alone, and a destination that differs is replaced only while
    /// it still holds what the previous run wrote; otherwise it is kept
    /// as a conflict unless `--overwrite`.
    ///
    /// # Errors
    ///
    /// The config file's or the secrets store's errors; nothing of the
    /// config is written then.
    pub fn apply(&self, dest: &Dest<'_>, entries: &mut [Entry]) -> Result<(), Error> {
        let ledger = dest.ledger.borrow().clone();
        let records = self.write(dest, entries, dest.overwrite, &ledger)?;
        let mut ledger = dest.ledger.borrow_mut();
        for (key, hash, written) in records {
            ledger.record(key, hash, written);
        }
        Ok(())
    }

    /// Writes everything, replacing differing values: for lists the
    /// component already merged with the destination's.
    ///
    /// # Errors
    ///
    /// As [`Writes::apply`].
    pub fn apply_merged(&self, dest: &Dest<'_>, entries: &mut [Entry]) -> Result<(), Error> {
        self.write(dest, entries, true, &Ledger::detached())
            .map(drop)
    }

    fn write(
        &self,
        dest: &Dest<'_>,
        entries: &mut [Entry],
        overwrite: bool,
        ledger: &Ledger,
    ) -> Result<Vec<(String, String, String)>, Error> {
        let scope = dest.scope;
        let store = scope.secrets().store();
        let mut current = BTreeMap::new();
        for s in &self.secrets {
            if let Some(value) = store.get(&s.name)? {
                current.insert(s.name.clone(), value.expose().to_owned());
            }
        }
        let file = scope.profile().config_file();
        let text = file.raw()?.unwrap_or_default();
        let pending = self.pending(&text, &current, entries, overwrite, ledger);
        if !pending.keys.is_empty() {
            dest.keep(file.path())?;
            file.set_values(&pending.keys)?;
        }
        for s in pending.secrets {
            dest.keep_secret(&s.name)?;
            store.set(&s.name, &s.value)?;
        }
        Ok(pending.records)
    }

    /// What a dry run predicts: the conflicts with an existing profile,
    /// read without writing (`config.toml`, and `secrets.toml` of the
    /// file backend).
    pub fn predict(&self, existing: &Existing<'_>, entries: &mut [Entry]) {
        let dir = existing.dir;
        let text = std::fs::read_to_string(dir.join("config.toml")).unwrap_or_default();
        let secrets: BTreeMap<String, String> = std::fs::read_to_string(dir.join("secrets.toml"))
            .ok()
            .and_then(|t| toml::from_str::<toml::Table>(&t).ok())
            .map(|t| {
                t.into_iter()
                    .filter_map(|(k, v)| v.as_str().map(|v| (k, v.to_owned())))
                    .collect()
            })
            .unwrap_or_default();
        let _ = self.pending(
            &text,
            &secrets,
            entries,
            existing.overwrite,
            existing.ledger,
        );
    }

    /// The keys and secrets to write given the destination's `config`
    /// text and `secrets`, and the ledger records of what gets written
    /// or already matches; the other entries become conflicts or are left
    /// alone.
    fn pending(
        &self,
        config: &str,
        secrets: &BTreeMap<String, String>,
        entries: &mut [Entry],
        overwrite: bool,
        ledger: &Ledger,
    ) -> Pending<'_> {
        let mut out = Pending::default();
        for s in &self.secrets {
            let key = format!("secret:{}", s.name);
            let new = digest(s.value.expose().as_bytes());
            let current = secrets.get(&s.name).map(|v| digest(v.as_bytes()));
            let decision = decide(ledger.get(&key), &new, &new, current.as_deref(), overwrite);
            if settle(
                decision,
                &mut entries[s.entry],
                "the secrets store holds another value",
            ) {
                out.secrets.push(s);
            }
            if matches!(decision, Decision::Write | Decision::Same) {
                out.records.push((key, new.clone(), new));
            }
        }
        let doc: toml::Table = toml::from_str(config).unwrap_or_default();
        for k in &self.config {
            let key = format!("config:{}", k.key);
            let new = normalize(&k.value).map_or_else(|| k.value.to_string(), |v| v.to_string());
            let current = lookup_value(&doc, &k.key).map(ToString::to_string);
            let hash = digest(new.as_bytes());
            let decision = decide(ledger.get(&key), &hash, &new, current.as_deref(), overwrite);
            let why = format!("config.toml already sets {}", k.key);
            if settle(decision, &mut entries[k.entry], &why) {
                out.keys.push((k.key.clone(), k.value.clone()));
            }
            if matches!(decision, Decision::Write | Decision::Same) {
                out.records.push((key, hash, new));
            }
        }
        out
    }
}

/// What [`Writes::pending`] found to do.
#[derive(Default)]
struct Pending<'a> {
    keys: Vec<(String, Value)>,
    secrets: Vec<&'a SecretWrite>,
    /// Ledger key, source hash and written value of each item written or
    /// matching.
    records: Vec<(String, String, String)>,
}

/// Applies `decision` to `entry`; `true` when the value must be written.
pub(crate) fn settle(decision: Decision, entry: &mut Entry, conflict_why: &str) -> bool {
    match decision {
        Decision::Write => true,
        Decision::Same => false,
        Decision::Unchanged => {
            unchanged(entry);
            false
        }
        Decision::Conflict => {
            conflict(entry, conflict_why);
            false
        }
    }
}

/// Marks `entry` as left alone by a re-run.
pub(crate) fn unchanged(entry: &mut Entry) {
    if !matches!(
        entry.disposition,
        Disposition::Failed | Disposition::Unsupported | Disposition::Conflict
    ) {
        entry.disposition = Disposition::Skipped;
        UNCHANGED.clone_into(&mut entry.reason);
    }
}

/// The value at dotted `key` of a parsed `config.toml`.
fn lookup_value<'a>(doc: &'a toml::Table, key: &str) -> Option<&'a toml::Value> {
    let mut segments = key.split('.');
    let mut value = doc.get(segments.next()?)?;
    for segment in segments {
        value = value.as_table()?.get(segment)?;
    }
    Some(value)
}

pub(crate) fn conflict(entry: &mut Entry, why: &str) {
    entry.disposition = Disposition::Conflict;
    entry.reason = format!("{why}; kept (use --overwrite to replace it)");
}

/// `v` as a plain TOML value, so formatting differences compare equal.
fn normalize(v: &Value) -> Option<toml::Value> {
    let mut table: toml::Table = toml::from_str(&format!("v = {v}")).ok()?;
    table.remove("v")
}

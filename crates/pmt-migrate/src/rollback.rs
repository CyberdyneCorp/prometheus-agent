//! `prometheus migrate rollback --run <run-id>` (spec: hermes-migration,
//! Migration rollback): takes a migration run back from its journal
//! ([`crate::journal`]), newest item first. An item still holding what
//! the run wrote is removed (created by the run) or restored (replaced by
//! it); one changed in Prometheus since is kept and reported
//! `kept_modified`; one already gone is reported `gone`. The source's
//! ledger returns to its state before the run, so the source can be
//! imported again. A run can be rolled back only while no later run of
//! the same source followed it in that profile.

use std::path::{Path, PathBuf};

use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
use pmt_core::error::{Error, codes};
use pmt_core::id::SessionId;
use pmt_store::Store;
use pmt_store::legacy::LegacyLink;
use serde::Serialize;
use serde_json::Value;

use crate::journal::{self, Item, Journal};
use crate::ledger::digest;

/// What became of one item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    /// Created by the run: removed.
    Removed,
    /// Replaced by the run: its previous value is back.
    Restored,
    /// Changed in Prometheus after the run: kept.
    KeptModified,
    /// Already gone.
    Gone,
}

/// One item's outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Outcome {
    /// `config`, `secret`, `path`, `cron_job`, `memory`, `pairing`,
    /// `session` or `appended`.
    pub kind: &'static str,
    /// The item (key, name, path or id).
    pub item: String,
    /// What became of it.
    pub disposition: Disposition,
}

/// The rollback of one profile.
#[derive(Debug, Clone, Serialize)]
pub struct Rolled {
    /// The run.
    pub run: String,
    /// The profile.
    pub profile: String,
    /// Every item, newest first.
    pub items: Vec<Outcome>,
}

/// Rolls run `run` back in every profile of the home at
/// `prometheus_home` that holds its journal.
///
/// # Errors
///
/// `migrate.run_not_found`, `migrate.rollback_order` (nothing changed);
/// home, store and secrets errors.
pub fn rollback(prometheus_home: &Path, run: &str) -> Result<Vec<Rolled>, Error> {
    let home = Home::open(prometheus_home)?;
    let mut found = Vec::new();
    for name in home.profiles()? {
        let profile = home.profile(&name)?;
        if let Some(journal) = Journal::load(profile.path(), run)? {
            check_last(profile.path(), &journal)?;
            found.push((name, profile, journal));
        }
    }
    if found.is_empty() {
        return Err(Error::new(
            &codes::MIGRATE_RUN_NOT_FOUND,
            format!(
                "no migration run '{run}' to roll back in {}",
                prometheus_home.display()
            ),
        ));
    }
    found
        .into_iter()
        .map(|(name, profile, journal)| one(&name, profile, &journal))
        .collect()
}

/// Refuses when a later run of the same source wrote into the profile.
fn check_last(profile_dir: &Path, journal: &Journal) -> Result<(), Error> {
    let runs = profile_dir.join("migrations").join("runs");
    let Ok(entries) = std::fs::read_dir(&runs) else {
        return Ok(());
    };
    let ledger = journal.ledger_before().0;
    for entry in entries.filter_map(Result::ok) {
        let other = entry.file_name().to_string_lossy().into_owned();
        // Run ids are lower-case ULIDs: later runs sort after.
        if other.as_str() <= journal.run() {
            continue;
        }
        if let Some(later) = Journal::load(profile_dir, &other)?
            && later.ledger_before().0 == ledger
        {
            return Err(Error::new(
                &codes::MIGRATE_ROLLBACK_ORDER,
                format!(
                    "run {other} migrated the same source after run {}; roll it back first",
                    journal.run()
                ),
            ));
        }
    }
    Ok(())
}

fn one(
    name: &ProfileName,
    profile: pmt_config::ProfileDir,
    journal: &Journal,
) -> Result<Rolled, Error> {
    let scope = ProfileScope::open(profile, &Overrides::default(), Launch::Other)?;
    let store = Store::open(&scope)?;
    let ctx = Ctx {
        scope: &scope,
        store: &store,
        copies: journal.location().join("copies"),
    };
    let mut items = Vec::new();
    for item in journal.items().iter().rev() {
        items.push(ctx.undo(item)?);
    }
    restore_ledger(journal)?;
    crate::undo::remove_any(journal.location())?;
    Ok(Rolled {
        run: journal.run().to_owned(),
        profile: name.as_str().to_owned(),
        items,
    })
}

/// Removes the tables around `key` that its removal left empty (the run
/// created them; a table someone keeps holds other keys).
fn prune(file: &pmt_config::ConfigFile, key: &str) -> Result<(), Error> {
    let text = file.raw()?.unwrap_or_default();
    let Ok(mut doc) = text.parse::<toml_edit::DocumentMut>() else {
        return Ok(());
    };
    let segments: Vec<&str> = key.split('.').collect();
    let mut changed = false;
    for depth in (1..segments.len()).rev() {
        let (parent, name) = (&segments[..depth - 1], segments[depth - 1]);
        let mut table = doc.as_table_mut() as &mut dyn toml_edit::TableLike;
        for segment in parent {
            match table
                .get_mut(segment)
                .and_then(toml_edit::Item::as_table_like_mut)
            {
                Some(next) => table = next,
                None => return Ok(()),
            }
        }
        let empty = table
            .get(name)
            .and_then(toml_edit::Item::as_table_like)
            .is_some_and(toml_edit::TableLike::is_empty);
        if !empty {
            break;
        }
        table.remove(name);
        changed = true;
    }
    if changed {
        file.replace(&doc.to_string())?;
    }
    Ok(())
}

fn restore_ledger(journal: &Journal) -> Result<(), Error> {
    let (Some(path), before) = journal.ledger_before() else {
        return Ok(());
    };
    match before {
        Some(text) => pmt_core::private_fs::write_atomic(path, text.as_bytes())
            .map_err(|e| Error::io(&codes::MIGRATE_DESTINATION, path, &e)),
        None => crate::undo::remove_any(path),
    }
}

struct Ctx<'a> {
    scope: &'a ProfileScope,
    store: &'a Store,
    copies: PathBuf,
}

fn outcome(kind: &'static str, item: impl Into<String>, disposition: Disposition) -> Outcome {
    Outcome {
        kind,
        item: item.into(),
        disposition,
    }
}

/// `Removed` when `before` was absent, else `Restored`.
fn back<T>(before: Option<&T>) -> Disposition {
    if before.is_some() {
        Disposition::Restored
    } else {
        Disposition::Removed
    }
}

impl Ctx<'_> {
    fn undo(&self, item: &Item) -> Result<Outcome, Error> {
        match item {
            Item::Config {
                key,
                before,
                written,
            } => self.config(key, before.as_deref(), written),
            Item::Secret {
                name,
                backup,
                written,
            } => self.secret(name, backup.as_deref(), written.as_deref()),
            Item::Path {
                path,
                copy,
                written,
            } => self.path(path, copy.as_deref(), written.as_deref()),
            Item::CronJob {
                id,
                before,
                written,
            } => self.cron_job(id, before.as_ref(), written.as_deref()),
            Item::Memory { id, content } => self.memory(id, content),
            Item::Pairing {
                platform,
                user,
                before,
                after,
            } => self.pairing((platform, user), before.as_ref(), after.as_ref()),
            Item::Session { id, messages } => self.session(id, *messages),
            Item::Appended {
                id,
                source,
                legacy_id,
                carried,
                counts,
            } => self.appended(id, (source, legacy_id, *carried), *counts),
        }
    }

    fn config(&self, key: &str, before: Option<&str>, written: &str) -> Result<Outcome, Error> {
        let file = self.scope.profile().config_file();
        let text = file.raw()?.unwrap_or_default();
        let doc: toml::Table = toml::from_str(&text).unwrap_or_default();
        let current = crate::writes::lookup_value(&doc, key).map(ToString::to_string);
        if current.as_deref() != Some(written) {
            let gone = current.is_none();
            return Ok(outcome(
                "config",
                key,
                if gone {
                    Disposition::Gone
                } else {
                    Disposition::KeptModified
                },
            ));
        }
        if let Some(value) = before {
            let value: toml_edit::Value = value.parse().map_err(|e| {
                Error::new(
                    &codes::MIGRATE_DESTINATION,
                    format!("the journal's previous value of {key} is not TOML: {e}"),
                )
            })?;
            file.set_values(&[(key.to_owned(), value)])?;
        } else {
            file.unset(key)?;
            prune(&file, key)?;
        }
        Ok(outcome("config", key, back(before.as_ref())))
    }

    fn secret(
        &self,
        name: &str,
        backup: Option<&str>,
        written: Option<&str>,
    ) -> Result<Outcome, Error> {
        let store = self.scope.secrets().store();
        let current = store.get(name)?.map(|v| digest(v.expose().as_bytes()));
        let restore = backup.map(|b| store.get(b)).transpose()?.flatten();
        if let Some(backup) = backup {
            store.remove(backup)?;
        }
        if current.as_deref() != written || current.is_none() {
            let gone = current.is_none();
            return Ok(outcome(
                "secret",
                name,
                if gone {
                    Disposition::Gone
                } else {
                    Disposition::KeptModified
                },
            ));
        }
        match &restore {
            Some(value) => store.set(name, value)?,
            None => {
                store.remove(name)?;
            }
        }
        Ok(outcome("secret", name, back(restore.as_ref())))
    }

    fn path(
        &self,
        path: &Path,
        copy: Option<&str>,
        written: Option<&str>,
    ) -> Result<Outcome, Error> {
        let label = path.display().to_string();
        let current = journal::tree_digest(path);
        if current.as_deref() != written {
            let gone = current.is_none();
            return Ok(outcome(
                "path",
                label,
                if gone {
                    Disposition::Gone
                } else {
                    Disposition::KeptModified
                },
            ));
        }
        crate::undo::remove_any(path)?;
        if let Some(copy) = copy {
            crate::undo::copy_any(&self.copies.join(copy), path)?;
        }
        Ok(outcome("path", label, back(copy.as_ref())))
    }

    fn cron_job(
        &self,
        id: &str,
        before: Option<&Value>,
        written: Option<&str>,
    ) -> Result<Outcome, Error> {
        let current = journal::job_digest(self.store, id)?;
        if current.is_none() || current.as_deref() != written {
            let gone = current.is_none();
            return Ok(outcome(
                "cron_job",
                id,
                if gone {
                    Disposition::Gone
                } else {
                    Disposition::KeptModified
                },
            ));
        }
        match before {
            Some(spec) => {
                let spec: pmt_cron::job::JobSpec =
                    serde_json::from_value(spec.clone()).map_err(|e| {
                        Error::new(&codes::MIGRATE_DESTINATION, format!("job {id}: {e}"))
                    })?;
                self.store.cron_modify_job(id, |row| {
                    row.spec = pmt_cron::job::spec_json(&spec);
                    Ok(())
                })?;
            }
            None => {
                self.store.cron_delete_job(id)?;
            }
        }
        Ok(outcome("cron_job", id, back(before)))
    }

    fn memory(&self, id: &str, content: &str) -> Result<Outcome, Error> {
        let mut disposition = Disposition::Gone;
        self.store.memory_batch(None, |batch| {
            if let Some(at) = batch.entries.iter().position(|e| e.id == id) {
                if batch.entries[at].content == content {
                    batch.entries.remove(at);
                    disposition = Disposition::Removed;
                } else {
                    disposition = Disposition::KeptModified;
                }
            }
            Ok(())
        })?;
        Ok(outcome("memory", id, disposition))
    }

    fn pairing(
        &self,
        (platform, user): (&str, &str),
        before: Option<&Value>,
        after: Option<&Value>,
    ) -> Result<Outcome, Error> {
        let label = format!("{platform}:{user}");
        let current = self
            .store
            .pairing_row(platform, user)?
            .map(|row| serde_json::to_value(row).unwrap_or(Value::Null));
        if current.as_ref() != after {
            let gone = current.is_none();
            return Ok(outcome(
                "pairing",
                label,
                if gone {
                    Disposition::Gone
                } else {
                    Disposition::KeptModified
                },
            ));
        }
        let row = before
            .map(|v| serde_json::from_value(v.clone()))
            .transpose()
            .map_err(|e| {
                Error::new(&codes::MIGRATE_DESTINATION, format!("pairing {label}: {e}"))
            })?;
        self.store.restore_pairing(platform, user, row.as_ref())?;
        Ok(outcome("pairing", label, back(before)))
    }

    fn session(&self, id: &str, messages: u64) -> Result<Outcome, Error> {
        let session = SessionId::parse(id)?;
        let current = match self.store.session(session) {
            Ok(row) => row,
            Err(e) if e.code().code == codes::SESSION_NOT_FOUND.code => {
                return Ok(outcome("session", id, Disposition::Gone));
            }
            Err(e) => return Err(e),
        };
        if current.message_count != messages {
            return Ok(outcome("session", id, Disposition::KeptModified));
        }
        self.store.delete_session(session, false)?;
        Ok(outcome("session", id, Disposition::Removed))
    }

    fn appended(
        &self,
        id: &str,
        (source, legacy_id, carried): (&str, &str, u64),
        (before, after): (u64, u64),
    ) -> Result<Outcome, Error> {
        let session = SessionId::parse(id)?;
        let current = match self.store.session(session) {
            Ok(row) => row,
            Err(e) if e.code().code == codes::SESSION_NOT_FOUND.code => {
                return Ok(outcome("appended", id, Disposition::Gone));
            }
            Err(e) => return Err(e),
        };
        if current.message_count != after {
            return Ok(outcome("appended", id, Disposition::KeptModified));
        }
        let link = LegacyLink {
            source: source.to_owned(),
            legacy_id: legacy_id.to_owned(),
            session,
            messages: carried,
        };
        self.store.retract_linked(&link, before)?;
        Ok(outcome("appended", id, Disposition::Restored))
    }
}

//! The rollback journal of a migration run (spec: hermes-migration,
//! Migration rollback): what each committed component created or
//! replaced, kept under `<profile>/migrations/runs/<run-id>/` so
//! `prometheus migrate rollback --run <run-id>` can take the run back
//! ([`crate::rollback`]).
//!
//! Every item records what the run wrote, so a rollback can tell an item
//! changed in Prometheus afterwards (kept, `kept_modified`) from one it may
//! remove or restore: config keys with their previous TOML value, secrets
//! (a replaced value is kept in the profile's secrets store under a
//! backup name, never in a file), files and directories with a copy of
//! what they replaced, scheduled jobs with their previous definition,
//! memory entries, sender approvals and imported sessions. The ledger as
//! it was before the run is kept too, so a rolled-back run can be imported
//! again.
//!
//! Items are collected per component and join the journal only when the
//! component commits; an undone component leaves nothing in it.

use std::path::{Path, PathBuf};

use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_store::Store;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::ledger::digest;

/// One item a run wrote.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Item {
    /// A `config.toml` key.
    Config {
        /// The dotted key.
        key: String,
        /// Its TOML value before the run (`None`: unset).
        before: Option<String>,
        /// The TOML value the run wrote.
        written: String,
    },
    /// A secret.
    Secret {
        /// The secret name.
        name: String,
        /// The secret holding its value before the run (`None`: unset).
        backup: Option<String>,
        /// Digest of the value the run wrote.
        written: Option<String>,
    },
    /// A file or directory of the profile.
    Path {
        /// Absolute.
        path: PathBuf,
        /// The copy of what it held before (`None`: absent), in the
        /// journal's directory.
        copy: Option<String>,
        /// Digest of what the run left (`None`: absent).
        written: Option<String>,
    },
    /// A scheduled job.
    CronJob {
        /// Its id.
        id: String,
        /// Its definition before the run (`None`: created by it).
        before: Option<Value>,
        /// Digest of the definition the run left.
        written: Option<String>,
    },
    /// A memory entry the run added.
    Memory {
        /// Its id.
        id: String,
        /// Its text.
        content: String,
    },
    /// A sender approval.
    Pairing {
        /// The platform.
        platform: String,
        /// The sender.
        user: String,
        /// The stored row before the run.
        before: Option<Value>,
        /// The stored row the run left.
        after: Option<Value>,
    },
    /// A session the run imported.
    Session {
        /// The Prometheus session.
        id: String,
        /// Its message count after the import.
        messages: u64,
    },
    /// Messages the run appended to a session an earlier run imported.
    Appended {
        /// The Prometheus session.
        id: String,
        /// The import's source (`hermes`).
        source: String,
        /// The session's id in the source.
        legacy_id: String,
        /// Source messages carried before the run.
        carried: u64,
        /// The session's message count before and after the run.
        counts: (u64, u64),
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct File {
    run: String,
    /// The ledger file of the run's source and its text before the run.
    ledger: Option<PathBuf>,
    ledger_before: Option<String>,
    items: Vec<Item>,
}

/// The directory of run `run`'s journal in `profile_dir`.
#[must_use]
pub fn dir(profile_dir: &Path, run: &str) -> PathBuf {
    profile_dir.join("migrations").join("runs").join(run)
}

const JOURNAL: &str = "journal.json";

/// The journal of one run into one profile.
#[derive(Debug)]
pub struct Journal {
    dir: PathBuf,
    file: File,
    /// The current component's items, not committed yet.
    pending: Vec<Item>,
}

fn io(path: &Path, e: &std::io::Error) -> Error {
    Error::io(&codes::MIGRATE_DESTINATION, path, e)
}

impl Journal {
    /// A new journal for run `run` into `profile_dir`, whose source's
    /// ledger lives at `ledger` (its current text is kept).
    #[must_use]
    pub fn start(profile_dir: &Path, run: &str, ledger: Option<&Path>) -> Self {
        Self {
            dir: dir(profile_dir, run),
            file: File {
                run: run.to_owned(),
                ledger: ledger.map(Path::to_path_buf),
                ledger_before: ledger.and_then(|p| std::fs::read_to_string(p).ok()),
                items: Vec::new(),
            },
            pending: Vec::new(),
        }
    }

    /// Reads run `run`'s journal in `profile_dir` (`None`: no such run
    /// there).
    ///
    /// # Errors
    ///
    /// `migrate.destination` when it exists but cannot be read.
    pub fn load(profile_dir: &Path, run: &str) -> Result<Option<Self>, Error> {
        let dir = dir(profile_dir, run);
        let path = dir.join(JOURNAL);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io(&path, &e)),
        };
        let file = serde_json::from_str(&text).map_err(|e| {
            Error::new(
                &codes::MIGRATE_DESTINATION,
                format!("the rollback journal {} is invalid: {e}", path.display()),
            )
        })?;
        Ok(Some(Self {
            dir,
            file,
            pending: Vec::new(),
        }))
    }

    /// The run.
    #[must_use]
    pub fn run(&self) -> &str {
        &self.file.run
    }

    /// The journal's directory.
    #[must_use]
    pub fn location(&self) -> &Path {
        &self.dir
    }

    /// The committed items, oldest first.
    #[must_use]
    pub fn items(&self) -> &[Item] {
        &self.file.items
    }

    /// The source's ledger and its text before the run.
    #[must_use]
    pub fn ledger_before(&self) -> (Option<&Path>, Option<&str>) {
        (
            self.file.ledger.as_deref(),
            self.file.ledger_before.as_deref(),
        )
    }

    /// Records a config key about to be written.
    pub fn config(&mut self, key: &str, before: Option<String>, written: String) {
        self.pending.push(Item::Config {
            key: key.to_owned(),
            before,
            written,
        });
    }

    /// Records the secret `name` about to be set: a value it holds is
    /// kept under a backup name in the same store.
    ///
    /// # Errors
    ///
    /// The store's errors: the caller must not write.
    pub fn secret(
        &mut self,
        store: &pmt_config::secrets::SecretStore,
        name: &str,
    ) -> Result<(), Error> {
        if self.pending_has(|i| matches!(i, Item::Secret { name: n, .. } if n == name)) {
            return Ok(());
        }
        let backup = match store.get(name)? {
            Some(value) => {
                let backup = backup_name(&self.file.run, name);
                store.set(&backup, &value)?;
                Some(backup)
            }
            None => None,
        };
        self.pending.push(Item::Secret {
            name: name.to_owned(),
            backup,
            written: None,
        });
        Ok(())
    }

    /// Records the file or directory at `path` about to be changed (a
    /// copy of it is made now).
    ///
    /// # Errors
    ///
    /// `migrate.destination` when the copy cannot be made.
    pub fn path(&mut self, path: &Path) -> Result<(), Error> {
        let path = crate::undo::created_from(path);
        let seen = |i: &Item| matches!(i, Item::Path { path: p, .. } if *p == path);
        if self.pending_has(seen) {
            return Ok(());
        }
        let copy = if std::fs::symlink_metadata(&path).is_ok() {
            let name = format!("copy-{}", self.file.items.len() + self.pending.len());
            crate::undo::copy_any(&path, &self.dir.join("copies").join(&name))?;
            Some(name)
        } else {
            None
        };
        self.pending.push(Item::Path {
            path,
            copy,
            written: None,
        });
        Ok(())
    }

    /// Records an item written to the store.
    pub fn item(&mut self, item: Item) {
        self.pending.push(item);
    }

    fn pending_has(&self, test: impl Fn(&Item) -> bool) -> bool {
        self.pending.iter().any(&test) || self.file.items.iter().any(test)
    }

    /// The current component committed: what it left is recorded and its
    /// items join the journal, which is saved.
    ///
    /// # Errors
    ///
    /// `migrate.destination` when the journal cannot be written.
    pub fn commit(&mut self, scope: &ProfileScope, store: &Store) -> Result<(), Error> {
        let mut items = std::mem::take(&mut self.pending);
        for item in &mut items {
            settle(item, scope, store)?;
        }
        self.file.items.append(&mut items);
        self.save()
    }

    /// The current component was undone: its items, copies and secret
    /// backups go.
    pub fn discard(&mut self, scope: &ProfileScope) {
        for item in std::mem::take(&mut self.pending) {
            match item {
                Item::Path {
                    copy: Some(copy), ..
                } => {
                    let _ = crate::undo::remove_any(&self.dir.join("copies").join(copy));
                }
                Item::Secret {
                    backup: Some(backup),
                    ..
                } => {
                    let _ = scope.secrets().store().remove(&backup);
                }
                _ => {}
            }
        }
    }

    fn save(&self) -> Result<(), Error> {
        std::fs::create_dir_all(&self.dir).map_err(|e| io(&self.dir, &e))?;
        let path = self.dir.join(JOURNAL);
        let text = serde_json::to_vec_pretty(&self.file).unwrap_or_default();
        pmt_core::private_fs::write_atomic(&path, &text).map_err(|e| io(&path, &e))
    }
}

/// Fills in what the run left of `item`.
fn settle(item: &mut Item, scope: &ProfileScope, store: &Store) -> Result<(), Error> {
    match item {
        Item::Secret { name, written, .. } => {
            *written = scope
                .secrets()
                .store()
                .get(name)?
                .map(|v| digest(v.expose().as_bytes()));
        }
        Item::Path { path, written, .. } => *written = tree_digest(path),
        Item::CronJob { id, written, .. } => *written = job_digest(store, id)?,
        _ => {}
    }
    Ok(())
}

/// The secret holding `name`'s value from before run `run`.
#[must_use]
pub fn backup_name(run: &str, name: &str) -> String {
    let full = format!("PMT_MIGRATE_UNDO_{}_{name}", run.to_ascii_uppercase());
    if full.len() <= 128 {
        return full;
    }
    format!(
        "PMT_MIGRATE_UNDO_{}",
        pmt_cron::job::hex(&Sha256::digest(full.as_bytes())).to_ascii_uppercase()
    )
}

/// Digest of the stored definition of job `id` (`None`: no such job).
///
/// # Errors
///
/// Store errors.
pub fn job_digest(store: &Store, id: &str) -> Result<Option<String>, Error> {
    let Some(row) = store.cron_job(id)? else {
        return Ok(None);
    };
    let job = pmt_cron::job::Job::from_row(row)?;
    Ok(Some(digest(pmt_cron::job::spec_json(&job.spec).as_bytes())))
}

/// Digest of the file, link or directory tree at `path` (`None`:
/// absent).
#[must_use]
pub fn tree_digest(path: &Path) -> Option<String> {
    let mut hash = Sha256::new();
    add(&mut hash, path, Path::new("")).ok()?;
    Some(format!("sha256:{}", pmt_cron::job::hex(&hash.finalize())))
}

fn add(hash: &mut Sha256, path: &Path, rel: &Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(path)?;
    hash.update(rel.to_string_lossy().as_bytes());
    if meta.is_symlink() {
        hash.update(b"\0link\0");
        hash.update(std::fs::read_link(path)?.to_string_lossy().as_bytes());
    } else if meta.is_dir() {
        hash.update(b"\0dir\0");
        let mut entries: Vec<_> = std::fs::read_dir(path)?.collect::<Result<_, _>>()?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            add(hash, &entry.path(), &rel.join(entry.file_name()))?;
        }
    } else {
        hash.update(b"\0file\0");
        hash.update(std::fs::read(path)?);
    }
    Ok(())
}

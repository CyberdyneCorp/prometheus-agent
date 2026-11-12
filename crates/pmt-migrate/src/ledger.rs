//! The migration ledger `<profile>/migrations/<source>-ledger.toml` (spec:
//! hermes-migration, Idempotent incremental re-run; Per-component
//! atomicity and resumability).
//!
//! It records the source it was written for and, per imported item, a
//! hash of the source content and what was written to the destination.
//! A re-run compares against it: an item whose source did not change is
//! left alone, and a destination value is replaced only while it still
//! equals what the previous run wrote, so edits made in Prometheus are
//! never lost. Each component is recorded once it committed, which is
//! what lets an interrupted run resume where it stopped.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use pmt_core::error::{Error, codes};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// One imported item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// Hash of the source content the item was imported from.
    pub hash: String,
    /// What was written: a TOML value, or a `sha256:` digest of a secret,
    /// a file or a directory.
    pub written: String,
}

/// A component that committed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Committed {
    /// The run that committed it last.
    pub run: String,
    /// When (RFC 3339).
    pub at: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct File {
    /// The source path.
    source: String,
    /// What identifies the source beyond its path.
    identity: String,
    #[serde(default)]
    components: BTreeMap<String, Committed>,
    #[serde(default)]
    items: BTreeMap<String, Record>,
}

/// A profile's ledger for one source kind.
#[derive(Debug, Clone)]
pub struct Ledger {
    path: Option<PathBuf>,
    file: File,
}

/// What [`decide`] concluded for one item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Write the new value.
    Write,
    /// The destination already holds it.
    Same,
    /// The source did not change since the last run: nothing to do (an
    /// edit or removal made in Prometheus stays).
    Unchanged,
    /// The destination was changed in Prometheus; it is kept.
    Conflict,
}

/// Decides one item: `prior` is the ledger record, `hash` the source
/// content's hash now, `new` what would be written and `current` what the
/// destination holds (`None`: absent).
#[must_use]
pub fn decide(
    prior: Option<&Record>,
    hash: &str,
    new: &str,
    current: Option<&str>,
    overwrite: bool,
) -> Decision {
    let unchanged = prior.is_some_and(|p| p.hash == hash);
    match current {
        _ if unchanged => Decision::Unchanged,
        Some(c) if c == new => Decision::Same,
        None => Decision::Write,
        Some(c) if prior.is_some_and(|p| p.written == c) || overwrite => Decision::Write,
        Some(_) => Decision::Conflict,
    }
}

/// The reason reported for an item a re-run left alone.
pub const UNCHANGED: &str = "unchanged since the last migration";

/// `sha256:<hex>` of `bytes`.
#[must_use]
pub fn digest(bytes: &[u8]) -> String {
    format!("sha256:{}", pmt_cron::job::hex(&Sha256::digest(bytes)))
}

/// The ledger file of `source` in `profile_dir`.
#[must_use]
pub fn path(profile_dir: &Path, source: &str) -> PathBuf {
    profile_dir
        .join("migrations")
        .join(format!("{source}-ledger.toml"))
}

impl Ledger {
    /// An empty ledger that is never saved (a dry run on a fresh machine).
    #[must_use]
    pub fn detached() -> Self {
        Self {
            path: None,
            file: File::default(),
        }
    }

    /// Reads the ledger of `source` in `profile_dir` for the source at
    /// `source_path` identified by `identity`; a ledger written for
    /// another source is set aside (`Some` warning) and every item counts
    /// as new.
    ///
    /// # Errors
    ///
    /// `migrate.component` when the ledger exists but is unreadable.
    pub fn load(
        profile_dir: &Path,
        source: &str,
        source_path: &Path,
        identity: &str,
    ) -> Result<(Self, Option<String>), Error> {
        let path = path(profile_dir, source);
        let fresh = File {
            source: source_path.display().to_string(),
            identity: identity.to_owned(),
            ..File::default()
        };
        let file: Option<File> = match std::fs::read_to_string(&path) {
            Ok(text) => Some(toml::from_str(&text).map_err(|e| {
                Error::new(
                    &codes::MIGRATE_COMPONENT,
                    format!("the migration ledger {} is invalid: {e}", path.display()),
                )
            })?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(Error::io(&codes::MIGRATE_COMPONENT, &path, &e)),
        };
        let mut warning = None;
        let file = match file {
            Some(f) if f.identity == identity => f,
            Some(f) => {
                warning = Some(format!(
                    "the migration ledger was written for {}; this run treats every item of {} as new",
                    f.source,
                    source_path.display()
                ));
                fresh
            }
            None => fresh,
        };
        Ok((
            Self {
                path: Some(path),
                file,
            },
            warning,
        ))
    }

    /// Reads a ledger without a source check (dry-run predictions).
    #[must_use]
    pub fn peek(profile_dir: &Path, source: &str) -> Self {
        let file = std::fs::read_to_string(path(profile_dir, source))
            .ok()
            .and_then(|t| toml::from_str(&t).ok())
            .unwrap_or_default();
        Self { path: None, file }
    }

    /// The record of `key`.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Record> {
        self.file.items.get(key)
    }

    /// Every record whose key starts with `prefix`.
    pub fn with_prefix<'a>(
        &'a self,
        prefix: &'a str,
    ) -> impl Iterator<Item = (&'a str, &'a Record)> + 'a {
        self.file
            .items
            .range(prefix.to_owned()..)
            .take_while(move |(k, _)| k.starts_with(prefix))
            .map(|(k, r)| (k.as_str(), r))
    }

    /// Records what was written for `key`.
    pub fn record(
        &mut self,
        key: impl Into<String>,
        hash: impl Into<String>,
        written: impl Into<String>,
    ) {
        self.file.items.insert(
            key.into(),
            Record {
                hash: hash.into(),
                written: written.into(),
            },
        );
    }

    /// When `component` last committed.
    #[must_use]
    pub fn committed(&self, component: &str) -> Option<&Committed> {
        self.file.components.get(component)
    }

    /// Marks `component` committed by `run` and saves the ledger.
    ///
    /// # Errors
    ///
    /// `home.io` when the ledger cannot be written.
    pub fn commit(&mut self, component: &str, run: &str) -> Result<(), Error> {
        self.file.components.insert(
            component.to_owned(),
            Committed {
                run: run.to_owned(),
                at: jiff::Timestamp::now().to_string(),
            },
        );
        self.save()
    }

    /// Saves the ledger (atomically, 0600).
    ///
    /// # Errors
    ///
    /// `home.io`.
    pub fn save(&self) -> Result<(), Error> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::io_doing(&codes::HOME_IO, "cannot create", parent, &e))?;
        }
        let text = toml::to_string(&self.file).map_err(|e| {
            Error::new(
                &codes::HOME_IO,
                format!("cannot serialize the migration ledger: {e}"),
            )
        })?;
        let header = "# Written by `prometheus migrate`: what each run imported. Editing it\n\
                      # makes the next run treat items as new or changed.\n";
        pmt_core::private_fs::write_atomic(path, format!("{header}{text}").as_bytes())
            .map_err(|e| Error::io_doing(&codes::HOME_IO, "cannot write", path, &e))
    }
}

/// What identifies a source directory beyond its path: its device and
/// inode on Unix (a home recreated at the same path is another source),
/// the canonical path elsewhere.
#[must_use]
pub fn identity(dir: &Path) -> String {
    let canonical = dir.canonicalize().unwrap_or_else(|_| dir.to_owned());
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let Ok(meta) = std::fs::metadata(&canonical) {
            return digest(
                format!("{}:{}:{}", canonical.display(), meta.dev(), meta.ino()).as_bytes(),
            );
        }
    }
    digest(canonical.display().to_string().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(hash: &str, written: &str) -> Record {
        Record {
            hash: hash.to_owned(),
            written: written.to_owned(),
        }
    }

    #[test]
    fn a_destination_still_holding_the_previous_value_is_updated() {
        let prior = rec("h60", "60");
        assert_eq!(
            decide(Some(&prior), "h90", "90", Some("60"), false),
            Decision::Write
        );
    }

    #[test]
    fn a_user_edit_is_kept_as_a_conflict_unless_overwrite() {
        let prior = rec("h60", "60");
        assert_eq!(
            decide(Some(&prior), "h90", "90", Some("80"), false),
            Decision::Conflict
        );
        assert_eq!(
            decide(Some(&prior), "h90", "90", Some("80"), true),
            Decision::Write
        );
    }

    #[test]
    fn an_unchanged_source_leaves_edits_and_removals_alone() {
        let prior = rec("h60", "60");
        assert_eq!(
            decide(Some(&prior), "h60", "60", Some("80"), true),
            Decision::Unchanged
        );
        assert_eq!(
            decide(Some(&prior), "h60", "60", None, false),
            Decision::Unchanged
        );
        assert_eq!(
            decide(Some(&prior), "h60", "60", Some("60"), false),
            Decision::Unchanged
        );
        assert_eq!(decide(None, "h", "1", Some("1"), false), Decision::Same);
        assert_eq!(decide(None, "h", "1", None, false), Decision::Write);
        assert_eq!(decide(None, "h", "1", Some("2"), false), Decision::Conflict);
    }

    #[test]
    fn a_ledger_round_trips_and_another_source_starts_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let (mut ledger, warning) =
            Ledger::load(dir.path(), "hermes", Path::new("/h"), "id-1").unwrap();
        assert!(warning.is_none());
        ledger.record("config:agent.max_iterations", "h", "60");
        ledger.commit("config", "run1").unwrap();
        let (again, _) = Ledger::load(dir.path(), "hermes", Path::new("/h"), "id-1").unwrap();
        assert_eq!(
            again.get("config:agent.max_iterations"),
            Some(&rec("h", "60"))
        );
        assert_eq!(again.committed("config").unwrap().run, "run1");
        let (other, warning) =
            Ledger::load(dir.path(), "hermes", Path::new("/h2"), "id-2").unwrap();
        assert!(warning.unwrap().contains("/h"));
        assert!(other.get("config:agent.max_iterations").is_none());
    }
}

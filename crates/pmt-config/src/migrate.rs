//! `config.toml` schema versioning (spec: configuration-and-profiles, Config
//! schema versioning and migration). A file older than [`SCHEMA_VERSION`]
//! is copied to `<profile>/backups/config/config-v<old>-<timestamp>.toml`
//! and migrated forward step by step at load time; a newer file is refused.
//!
//! Each step edits the format-preserving document, so comments and
//! untouched keys survive, and a renamed key is only written when its value
//! differs from the new default (the file keeps only deliberate choices).

use std::path::{Path, PathBuf};

use pmt_core::error::{Error, codes};
use pmt_core::private_fs;
use toml_edit::{DocumentMut, Item, Key};

use crate::catalog;
use crate::parse;
use crate::schema::SCHEMA_VERSION;

/// One forward step from version `from` to `from + 1`.
struct Migration {
    from: u32,
    apply: fn(&mut DocumentMut),
}

/// Every step, in order; `MIGRATIONS[n].from == n`.
const MIGRATIONS: &[Migration] = &[Migration {
    from: 0,
    apply: v0_to_v1,
}];

/// Version 0 is the pre-release draft format: durations had no unit
/// suffix and the turn limit was called `max_turns`.
fn v0_to_v1(doc: &mut DocumentMut) {
    rename(doc, "agent", "max_turns", "max_iterations");
    rename(doc, "approvals", "timeout", "timeout_secs");
}

/// A completed migration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Migrated {
    /// The file's version before.
    pub from: u32,
    /// The copy of the file before migration.
    pub backup: PathBuf,
}

/// The file's `schema_version`; a file without one is current.
///
/// # Errors
///
/// `config.invalid` when it is not a non-negative integer.
pub fn file_version(doc: &DocumentMut) -> Result<u32, Error> {
    match doc.get("schema_version") {
        None => Ok(SCHEMA_VERSION),
        Some(item) => item
            .as_integer()
            .and_then(|v| u32::try_from(v).ok())
            .ok_or_else(|| {
                Error::new(
                    &codes::CONFIG_INVALID,
                    "schema_version: expected a non-negative integer",
                )
            }),
    }
}

/// Brings `text` (the contents of `path`) to [`SCHEMA_VERSION`]: returns it
/// unchanged when current, else backs it up under `backups`, migrates it,
/// writes the result atomically to `path` and returns it.
///
/// # Errors
///
/// `config.parse` for invalid TOML, `config.too_new` (exit 78) for a file
/// from a newer binary, `home.io` when the backup or write fails.
pub(crate) fn upgrade(
    path: &Path,
    text: &str,
    backups: &Path,
) -> Result<(String, Option<Migrated>), Error> {
    let (version, migrated) = in_memory(path, text)?;
    if version == SCHEMA_VERSION {
        return Ok((text.to_owned(), None));
    }
    let backup = crate::backups::save(backups, &format!("config-v{version}"), text)?;
    private_fs::write_atomic(path, migrated.as_bytes()).map_err(|e| {
        Error::new(
            &codes::HOME_IO,
            format!("cannot write {}: {e}", path.display()),
        )
    })?;
    tracing::info!(
        "migrated {} from schema_version {version} to {SCHEMA_VERSION}; backup: {}",
        path.display(),
        backup.display()
    );
    Ok((
        migrated,
        Some(Migrated {
            from: version,
            backup,
        }),
    ))
}

/// The file's version and `text` brought to [`SCHEMA_VERSION`], touching
/// nothing on disk.
///
/// # Errors
///
/// `config.parse` for invalid TOML, `config.too_new` for a file from a
/// newer binary.
pub(crate) fn in_memory(path: &Path, text: &str) -> Result<(u32, String), Error> {
    let mut doc = parse::document(path, text)?.into_mut();
    let version = file_version(&doc)?;
    if version > SCHEMA_VERSION {
        return Err(Error::new(
            &codes::CONFIG_TOO_NEW,
            format!(
                "config.toml schema_version {version} is newer than this prometheus supports ({SCHEMA_VERSION}); upgrade prometheus"
            ),
        )
        .with_hint("run: prometheus update"));
    }
    if version == SCHEMA_VERSION {
        return Ok((version, text.to_owned()));
    }
    for step in MIGRATIONS.iter().filter(|step| step.from >= version) {
        (step.apply)(&mut doc);
    }
    doc["schema_version"] = toml_edit::value(i64::from(SCHEMA_VERSION));
    Ok((version, doc.to_string()))
}

/// Renames `[table] old` to `new`, keeping its comments; the value is
/// dropped when it equals the new key's default or `new` is already set.
fn rename(doc: &mut DocumentMut, table: &str, old: &str, new: &str) {
    let Some(parent) = doc.get_mut(table).and_then(Item::as_table_mut) else {
        return;
    };
    let Some((old_key, item)) = parent.remove_entry(old) else {
        return;
    };
    let is_default = item.as_value().is_some_and(|value| {
        let node = crate::validate::Node::from_value(value);
        let default = catalog::lookup(&format!("{table}.{new}")).and_then(|info| info.default);
        matches!((&node.kind, default), (crate::validate::Kind::Scalar(v, _), Some(d)) if v == d)
    });
    if is_default || parent.contains_key(new) {
        return;
    }
    let mut new_key = Key::new(new);
    *new_key.leaf_decor_mut() = old_key.leaf_decor().clone();
    *new_key.dotted_decor_mut() = old_key.dotted_decor().clone();
    parent.insert_formatted(&new_key, item);
}

#[cfg(test)]
mod tests {
    use super::{MIGRATIONS, upgrade};
    use crate::schema::SCHEMA_VERSION;

    #[test]
    fn steps_cover_every_version_in_order() {
        assert_eq!(MIGRATIONS.len(), SCHEMA_VERSION as usize);
        for (i, step) in MIGRATIONS.iter().enumerate() {
            assert_eq!(step.from as usize, i);
        }
    }

    #[test]
    fn v0_renames_keep_comments_and_drop_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let text = "schema_version = 0\n\n[agent]\n# my limit\nmax_turns = 40\n\n[approvals]\ntimeout = 300\nmode = \"smart\"\n";
        std::fs::write(&path, text).unwrap();
        let (out, migrated) = upgrade(&path, text, &dir.path().join("backups")).unwrap();
        assert_eq!(
            out,
            "schema_version = 1\n\n[agent]\n# my limit\nmax_iterations = 40\n\n[approvals]\nmode = \"smart\"\n"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), out);
        let migrated = migrated.unwrap();
        assert_eq!(migrated.from, 0);
        assert_eq!(std::fs::read_to_string(&migrated.backup).unwrap(), text);
    }

    #[test]
    fn current_file_is_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let (out, migrated) = upgrade(&path, "[agent]\n", &dir.path().join("b")).unwrap();
        assert_eq!(out, "[agent]\n");
        assert!(migrated.is_none());
        assert!(!dir.path().join("b").exists());
    }

    #[test]
    fn newer_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let err = upgrade(&path, "schema_version = 99\n", dir.path()).unwrap_err();
        assert_eq!(err.exit_code(), 78);
        assert_eq!(
            err.message(),
            format!(
                "config.toml schema_version 99 is newer than this prometheus supports ({SCHEMA_VERSION}); upgrade prometheus"
            )
        );
    }
}

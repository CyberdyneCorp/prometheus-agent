//! The offline conversion of a store between plaintext and encrypted
//! (`prometheus sessions encrypt|decrypt`; spec: session-storage,
//! Encryption at rest option): a consistent copy of `state.db` is written
//! beside it (`VACUUM INTO`, sealed or not by the VFS — never a plaintext
//! temporary of an encrypted store), every blob file is rewritten into a
//! new directory, and then both replace the originals with renames. The
//! caller holds the home's writer lock and records the new
//! `storage.encryption` afterwards.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pmt_config::ProfileScope;
use pmt_config::schema::StorageEncryption;
use pmt_config::secrets::Keychain;
use pmt_core::error::{Error, codes};

use super::{Crypt, keys};
use crate::error::sql;
use crate::location::StoreLocation;

/// What a conversion did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Converted {
    /// The database file.
    pub database: PathBuf,
    /// Blob files rewritten.
    pub blobs: usize,
}

fn io(path: &Path, e: &std::io::Error) -> Error {
    Error::io(&codes::STORE_IO, path, e)
}

/// The encryption the file at `database` is in now under `current`.
fn source(
    scope: &ProfileScope,
    keychain: &Arc<dyn Keychain>,
    current: StorageEncryption,
    database: &Path,
) -> Result<Option<Crypt>, Error> {
    match super::is_plaintext(database) {
        None => Err(Error::new(
            &codes::STORE_IO,
            format!("{} does not exist or is empty", database.display()),
        )),
        Some(true) => Ok(None),
        Some(false) if current == StorageEncryption::None => Err(Error::new(
            &codes::STORE_KEY_UNAVAILABLE,
            super::KEY_UNAVAILABLE,
        )
        .with_hint("storage.encryption is none; set it to the store's mode first")),
        Some(false) => keys::resolve(scope, keychain, current, database, false),
    }
}

/// Converts `scope`'s store, now in `current` (`storage.encryption`), to
/// `wanted`.
///
/// # Errors
///
/// `store.key_unavailable`, `store.io`, `store.sqlite`; nothing is
/// replaced unless every copy was written.
pub fn convert_store(
    scope: &ProfileScope,
    keychain: &Arc<dyn Keychain>,
    current: StorageEncryption,
    wanted: StorageEncryption,
) -> Result<Converted, Error> {
    let location = StoreLocation::of(scope);
    let database = location.database().to_path_buf();
    let from = source(scope, keychain, current, &database)?;
    let to = keys::resolve(scope, keychain, wanted, &database, true)?;
    let staged = sibling(&database, "converting");
    remove_if_present(&staged)?;
    {
        let conn = super::open(&database, from.as_ref()).map_err(sql)?;
        conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
            .map_err(sql)?;
        super::convert(&conn, &staged, to.as_ref()).map_err(sql)?;
    }
    let blobs = location.blobs().to_path_buf();
    let new_blobs = sibling(&blobs, "converting");
    let count = rewrite_blobs(&blobs, &new_blobs, from.as_ref(), to.as_ref())?;
    let old_blobs = sibling(&blobs, "old");
    remove_dir_if_present(&old_blobs)?;
    if blobs.exists() {
        std::fs::rename(&blobs, &old_blobs).map_err(|e| io(&blobs, &e))?;
    }
    std::fs::rename(&new_blobs, &blobs).map_err(|e| io(&new_blobs, &e))?;
    for suffix in ["-wal", "-shm"] {
        remove_if_present(&with_suffix(&database, suffix))?;
    }
    std::fs::rename(&staged, &database).map_err(|e| io(&database, &e))?;
    keys::mark(&database, wanted)?;
    remove_dir_if_present(&old_blobs)?;
    Ok(Converted {
        database,
        blobs: count,
    })
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

fn sibling(path: &Path, what: &str) -> PathBuf {
    with_suffix(path, &format!(".{what}"))
}

fn remove_if_present(path: &Path) -> Result<(), Error> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(io(path, &e)),
        _ => Ok(()),
    }
}

fn remove_dir_if_present(path: &Path) -> Result<(), Error> {
    match std::fs::remove_dir_all(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(io(path, &e)),
        _ => Ok(()),
    }
}

/// Every blob file of `from_dir` rewritten into `to_dir`: opened with
/// `from`'s key, sealed with `to`'s.
fn rewrite_blobs(
    from_dir: &Path,
    to_dir: &Path,
    from: Option<&Crypt>,
    to: Option<&Crypt>,
) -> Result<usize, Error> {
    remove_dir_if_present(to_dir)?;
    crate::files::private_dir(to_dir)?;
    let mut count = 0;
    let Ok(fans) = std::fs::read_dir(from_dir) else {
        return Ok(0);
    };
    for fan in fans.flatten().filter(|e| e.path().is_dir()) {
        let target = to_dir.join(fan.file_name());
        crate::files::private_dir(&target)?;
        for entry in std::fs::read_dir(fan.path()).map_err(|e| io(&fan.path(), &e))? {
            let path = entry.map_err(|e| io(&fan.path(), &e))?.path();
            if !path.is_file()
                || path
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with('.'))
            {
                continue;
            }
            let bytes = std::fs::read(&path).map_err(|e| io(&path, &e))?;
            let plain = match from {
                Some(c) => super::open_blob(c.key(), &bytes).map_err(|e| {
                    Error::new(
                        &codes::STORE_BLOB_CORRUPT,
                        format!("{}: {e}", path.display()),
                    )
                })?,
                None => bytes,
            };
            let out = match to {
                Some(c) => super::seal_blob(c.key(), &plain)
                    .map_err(|e| Error::new(&codes::STORE_IO, e))?,
                None => plain,
            };
            let dest = target.join(entry_name(&path));
            pmt_core::private_fs::write_atomic(&dest, &out).map_err(|e| io(&dest, &e))?;
            count += 1;
        }
    }
    Ok(count)
}

fn entry_name(path: &Path) -> std::ffi::OsString {
    path.file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_default()
}

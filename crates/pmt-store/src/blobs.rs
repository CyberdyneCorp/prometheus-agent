//! The content-addressed blob store `<profile>/blobs/<aa>/<sha256>` (spec:
//! session-storage, Content-addressed blob store with garbage collection).
//!
//! A blob is written (temp file + atomic rename) BEFORE the row that
//! references it commits, so a committed reference never dangles. The
//! price is that a crash between the two leaves an unreferenced blob; the
//! collector removes those, but only blobs older than [`GC_GRACE`], which
//! also protects a blob whose referencing transaction is still running.
//! References are counted in `blob_refs` (one row per referencing owner,
//! maintained by triggers for messages).

use std::fmt;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use pmt_core::error::{Error, codes};
use pmt_core::private_fs;
use rusqlite::OptionalExtension;
use sha2::{Digest, Sha256};

use crate::error::{self, sql};
use crate::files;
use crate::store::Store;

/// Payloads larger than this are moved out of rows into the blob store.
pub const SPILL_BYTES: usize = 64 * 1024;

/// Unreferenced blobs younger than this are never collected.
pub const GC_GRACE: Duration = Duration::from_hours(24);

const REF_PREFIX: &str = "blob:sha256:";

/// A reference to a blob: its SHA-256, written `blob:sha256:<hex>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BlobRef(String);

impl BlobRef {
    /// The reference of `bytes`.
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        Self(hex(&Sha256::digest(bytes)))
    }

    /// Parses `blob:sha256:<64 lower-case hex>`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let hex = text.strip_prefix(REF_PREFIX)?;
        is_sha256(hex).then(|| Self(hex.to_owned()))
    }

    /// The lower-case hex SHA-256.
    #[must_use]
    pub fn sha256(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BlobRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{REF_PREFIX}{}", self.0)
    }
}

/// What one collection pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GcReport {
    /// Unreferenced blobs older than the grace period, deleted.
    pub deleted: Vec<BlobRef>,
    /// Blobs kept because a canonical row references them.
    pub referenced: usize,
    /// Unreferenced blobs kept because they are younger than the grace.
    pub young: usize,
    /// Abandoned temp files removed.
    pub temp_files: usize,
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    use fmt::Write as _;
    bytes.iter().fold(String::with_capacity(64), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

fn is_sha256(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn blob_path(dir: &Path, sha256: &str) -> PathBuf {
    dir.join(&sha256[..2]).join(sha256)
}

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Writes `bytes` under `dir` unless the blob exists; an existing blob's
/// mtime is refreshed so a concurrent collection treats it as fresh.
pub(crate) fn put(dir: &Path, bytes: &[u8]) -> Result<BlobRef, Error> {
    let blob = BlobRef::of(bytes);
    let path = blob_path(dir, blob.sha256());
    if path.is_file() {
        let touched = std::fs::File::options()
            .append(true)
            .open(&path)
            .and_then(|file| file.set_modified(SystemTime::now()));
        if touched.is_ok() {
            return Ok(blob);
        }
    }
    let parent = path.parent().expect("blob paths have a fan-out directory");
    files::private_dir(parent)?;
    let temp = parent.join(format!(
        ".{}.tmp.{}.{}",
        blob.sha256(),
        std::process::id(),
        TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let written = private_fs::new_file_options()
        .open(&temp)
        .and_then(|mut file| {
            file.write_all(bytes)?;
            file.sync_all()
        })
        .and_then(|()| std::fs::rename(&temp, &path));
    if let Err(e) = written {
        let _ = std::fs::remove_file(&temp);
        return Err(error::io(&path, &e));
    }
    Ok(blob)
}

/// Reads and verifies a blob.
pub(crate) fn get(dir: &Path, blob: &BlobRef) -> Result<Vec<u8>, Error> {
    let path = blob_path(dir, blob.sha256());
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(Error::new(
                &codes::STORE_BLOB_MISSING,
                format!("{blob} is missing from {}", dir.display()),
            ));
        }
        Err(e) => return Err(error::io(&path, &e)),
    };
    if BlobRef::of(&bytes) != *blob {
        return Err(Error::new(
            &codes::STORE_BLOB_CORRUPT,
            format!("{} does not match its SHA-256 name", path.display()),
        ));
    }
    Ok(bytes)
}

impl Store {
    /// Stores `bytes` in the blob store. Reference it from a canonical row
    /// (a message payload, or [`Store::reference_blob`]) within
    /// [`GC_GRACE`], or it is collected.
    ///
    /// # Errors
    ///
    /// `store.io`.
    pub fn put_blob(&self, bytes: &[u8]) -> Result<BlobRef, Error> {
        put(self.location().blobs(), bytes)
    }

    /// Reads a blob, verifying its hash.
    ///
    /// # Errors
    ///
    /// `store.blob_missing`, `store.blob_corrupt`, `store.io`.
    pub fn get_blob(&self, blob: &BlobRef) -> Result<Vec<u8>, Error> {
        get(self.location().blobs(), blob)
    }

    /// Records that `owner_kind`/`owner_id` (an attachment, a trajectory...)
    /// references `blob`, keeping it from collection.
    ///
    /// # Errors
    ///
    /// `store.schema_too_new`, `store.io`, `store.sqlite`.
    pub fn reference_blob(
        &self,
        blob: &BlobRef,
        owner_kind: &str,
        owner_id: &str,
    ) -> Result<(), Error> {
        self.write(|tx| {
            tx.execute(
                "INSERT OR IGNORE INTO blob_refs (sha256, owner_kind, owner_id) VALUES (?1, ?2, ?3)",
                [blob.sha256(), owner_kind, owner_id],
            )
            .map(drop)
            .map_err(sql)
        })
    }

    /// Drops one reference recorded with [`Store::reference_blob`].
    ///
    /// # Errors
    ///
    /// `store.schema_too_new`, `store.io`, `store.sqlite`.
    pub fn release_blob(
        &self,
        blob: &BlobRef,
        owner_kind: &str,
        owner_id: &str,
    ) -> Result<(), Error> {
        self.write(|tx| {
            tx.execute(
                "DELETE FROM blob_refs WHERE sha256 = ?1 AND owner_kind = ?2 AND owner_id = ?3",
                [blob.sha256(), owner_kind, owner_id],
            )
            .map(drop)
            .map_err(sql)
        })
    }

    /// How many canonical rows reference `blob`.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn blob_references(&self, blob: &BlobRef) -> Result<u64, Error> {
        self.read(|conn| {
            conn.query_row(
                "SELECT count(*) FROM blob_refs WHERE sha256 = ?1",
                [blob.sha256()],
                |row| row.get::<_, i64>(0),
            )
            .map(|count| u64::try_from(count).unwrap_or(0))
            .map_err(sql)
        })
    }

    /// Maintenance only: deletes blobs no canonical row references once
    /// they are older than [`GC_GRACE`], and abandoned temp files.
    ///
    /// # Errors
    ///
    /// `store.schema_too_new` (a newer build owns the references),
    /// `store.io`, `store.sqlite`.
    pub fn collect_blobs(&self) -> Result<GcReport, Error> {
        self.ensure_writable()?;
        let dir = self.location().blobs();
        let mut report = GcReport::default();
        for path in blob_files(dir)? {
            self.collect_one(&path, &mut report)?;
        }
        Ok(report)
    }

    fn collect_one(&self, path: &Path, report: &mut GcReport) -> Result<(), Error> {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if name.starts_with('.') {
            if older_than_grace(path) {
                std::fs::remove_file(path).map_err(|e| error::io(path, &e))?;
                report.temp_files += 1;
            }
            return Ok(());
        }
        if !is_sha256(&name) {
            return Ok(());
        }
        let blob = BlobRef(name.into_owned());
        let referenced = self.read(|conn| {
            conn.query_row(
                "SELECT 1 FROM blob_refs WHERE sha256 = ?1 LIMIT 1",
                [blob.sha256()],
                |_| Ok(()),
            )
            .optional()
            .map_err(sql)
        })?;
        if referenced.is_some() {
            report.referenced += 1;
        } else if older_than_grace(path) {
            std::fs::remove_file(path).map_err(|e| error::io(path, &e))?;
            report.deleted.push(blob);
        } else {
            report.young += 1;
        }
        Ok(())
    }
}

fn older_than_grace(path: &Path) -> bool {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age > GC_GRACE)
}

/// Every file one level below the fan-out directories.
fn blob_files(dir: &Path) -> Result<Vec<PathBuf>, Error> {
    let mut found = Vec::new();
    let fan_out = std::fs::read_dir(dir).map_err(|e| error::io(dir, &e))?;
    for entry in fan_out.filter_map(Result::ok) {
        let sub = entry.path();
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let files = std::fs::read_dir(&sub).map_err(|e| error::io(&sub, &e))?;
        found.extend(
            files
                .filter_map(Result::ok)
                .filter(|f| f.file_type().is_ok_and(|t| t.is_file()))
                .map(|f| f.path()),
        );
    }
    Ok(found)
}

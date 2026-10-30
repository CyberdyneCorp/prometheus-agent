//! `prometheus backup create` (spec: configuration-and-profiles, Whole-home
//! backup): a `.tar.zst` of the home with a hashed manifest as its last
//! entry. Databases are first copied through the SQLite online backup API
//! into a staging directory; every other member is streamed and hashed as
//! it is written.

use std::fs::File;
use std::io::{self, BufWriter, Write as _};
use std::path::{Path, PathBuf};

use pmt_config::Home;
use pmt_core::error::{Error, codes};
use pmt_core::private_fs;
use serde::Serialize;

use crate::fsutil::{self, Hashing, io_error};
use crate::manifest::{self, Manifest, Member};
use crate::{members, sqlite};

/// Default-named archives: `prometheus-<UTC timestamp>[-<label>].tar.zst`.
const PREFIX: &str = "prometheus-";
const SUFFIX: &str = ".tar.zst";
/// zstd level: fast, still a good ratio for text and SQLite pages.
const LEVEL: i32 = 3;

/// `backup create` options.
#[derive(Debug, Clone, Default)]
pub struct CreateOptions {
    /// `-o`: where to write; default `<home>/backups/<default name>`.
    pub output: Option<PathBuf>,
    /// `--quick`: critical state only.
    pub quick: bool,
    /// `--label`.
    pub label: Option<String>,
    /// `--keep`: default-named backups kept after this one (default 3).
    pub keep: usize,
}

/// What `backup create` wrote.
#[derive(Debug, Clone, Serialize)]
pub struct Created {
    /// The archive.
    pub path: PathBuf,
    /// Members archived.
    pub members: usize,
    /// Their total size.
    pub bytes: u64,
    /// Members that could not be read (the command then exits 5).
    pub skipped: Vec<String>,
    /// Older default-named backups removed by `--keep`.
    pub pruned: Vec<PathBuf>,
}

/// Writes a backup of `home`.
///
/// # Errors
///
/// `backup.running` (75) when another backup holds the lock, `backup.io`.
pub fn create(home: &Home, options: &CreateOptions) -> Result<Created, Error> {
    let backups = home.root().join("backups");
    private_fs::create_dir(&backups).map_err(|e| io_error(&backups, &e))?;
    let _lock = lock(&backups)?;
    let now = jiff::Timestamp::now();
    let output = match &options.output {
        Some(path) => path.clone(),
        None => backups.join(default_name(now, options.label.as_deref())),
    };
    let quick = options.quick;
    let (selected, mut skipped) = members::walk(home.root(), &|m| !quick || members::quick(m));
    let staging = Staging::new(&backups)?;
    let partial = output.with_extension("zst.partial");
    let manifest = Manifest {
        format: manifest::FORMAT,
        created_at: now.to_string(),
        prometheus_version: env!("CARGO_PKG_VERSION").to_owned(),
        label: options.label.clone(),
        quick,
        members: Vec::new(),
    };
    let written = write_archive(
        home.root(),
        &staging,
        &selected,
        &partial,
        &mut skipped,
        manifest,
    )
    .and_then(|manifest| {
        std::fs::rename(&partial, &output).map_err(|e| io_error(&output, &e))?;
        Ok(manifest)
    });
    let manifest = match written {
        Ok(manifest) => manifest,
        Err(err) => {
            let _ = std::fs::remove_file(&partial);
            return Err(err);
        }
    };
    let pruned = prune(&backups, options.keep, &output)?;
    Ok(Created {
        path: output,
        members: manifest.members.len(),
        bytes: manifest.members.iter().map(|m| m.size).sum(),
        skipped,
        pruned,
    })
}

/// `prometheus-<YYYYMMDDTHHMMSSZ>[-<label>].tar.zst`.
fn default_name(now: jiff::Timestamp, label: Option<&str>) -> String {
    let stamp = now.strftime("%Y%m%dT%H%M%SZ");
    let label: String = label
        .map(|l| {
            let clean: String = l
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                        c
                    } else {
                        '-'
                    }
                })
                .collect();
            format!("-{clean}")
        })
        .unwrap_or_default();
    format!("{PREFIX}{stamp}{label}{SUFFIX}")
}

fn lock(backups: &Path) -> Result<File, Error> {
    let path = backups.join(".backup.lock");
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| io_error(&path, &e))?;
    fsutil::private_file(&path);
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => Err(Error::new(
            &codes::BACKUP_RUNNING,
            "another backup is running",
        )),
        Err(std::fs::TryLockError::Error(e)) => Err(io_error(&path, &e)),
    }
}

/// A private scratch directory removed on drop.
pub(crate) struct Staging {
    dir: PathBuf,
}

impl Staging {
    pub(crate) fn new(parent: &Path) -> Result<Self, Error> {
        let dir = parent.join(format!(
            ".staging-{}",
            pmt_core::id::SnapshotId::new().ulid()
        ));
        private_fs::create_dir(&dir).map_err(|e| io_error(&dir, &e))?;
        Ok(Self { dir })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.dir
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

type Encoder = zstd::Encoder<'static, BufWriter<File>>;

/// Writes every selected member, then the manifest, to `partial`.
fn write_archive(
    root: &Path,
    staging: &Staging,
    selected: &[String],
    partial: &Path,
    skipped: &mut Vec<String>,
    mut manifest: Manifest,
) -> Result<Manifest, Error> {
    let file = private_fs::new_file_options()
        .open(partial)
        .map_err(|e| io_error(partial, &e))?;
    let encoder =
        zstd::Encoder::new(BufWriter::new(file), LEVEL).map_err(|e| io_error(partial, &e))?;
    let mut tar = tar::Builder::new(encoder);
    for (index, relative) in selected.iter().enumerate() {
        let source = members::on_disk(root, relative);
        let source = if members::is_database(relative) {
            let copy = staging.path().join(format!("{index}.db"));
            if let Err(err) = sqlite::copy(&source, &copy) {
                tracing::warn!("{err}");
                skipped.push(relative.clone());
                continue;
            }
            copy
        } else {
            source
        };
        match append(&mut tar, &source, relative)? {
            Some(member) => manifest.members.push(member),
            None => skipped.push(relative.clone()),
        }
    }
    let json = serde_json::to_vec_pretty(&manifest).expect("a manifest serializes");
    let mut header = tar::Header::new_gnu();
    header.set_size(json.len() as u64);
    header.set_mode(0o600);
    tar.append_data(&mut header, manifest::ARCHIVE_ENTRY, json.as_slice())
        .map_err(|e| io_error(partial, &e))?;
    let mut out = tar
        .into_inner()
        .and_then(zstd::Encoder::finish)
        .map_err(|e| io_error(partial, &e))?;
    out.flush().map_err(|e| io_error(partial, &e))?;
    out.get_ref()
        .sync_all()
        .map_err(|e| io_error(partial, &e))?;
    Ok(manifest)
}

/// Appends one file, hashing it as it streams; `None` when it cannot be
/// opened (an unreadable member is skipped and reported).
fn append(
    tar: &mut tar::Builder<Encoder>,
    source: &Path,
    relative: &str,
) -> Result<Option<Member>, Error> {
    let Ok(file) = File::open(source) else {
        return Ok(None);
    };
    let size = file.metadata().map_err(|e| io_error(source, &e))?.len();
    let mut header = tar::Header::new_gnu();
    header.set_size(size);
    header.set_mode(0o600);
    header.set_entry_type(tar::EntryType::Regular);
    let mut reader = Hashing::new(io::Read::take(file, size));
    tar.append_data(&mut header, relative, &mut reader)
        .map_err(|e| io_error(source, &e))?;
    let (read, sha256) = reader.finish();
    if read != size {
        return Err(io_error(
            source,
            &"changed size while it was archived; retry the backup",
        ));
    }
    Ok(Some(Member {
        path: relative.to_owned(),
        size,
        sha256,
    }))
}

/// Removes the oldest default-named archives in `backups` beyond `keep`
/// (never `just_written`, never other files).
fn prune(backups: &Path, keep: usize, just_written: &Path) -> Result<Vec<PathBuf>, Error> {
    let entries = std::fs::read_dir(backups).map_err(|e| io_error(backups, &e))?;
    let mut named: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(PREFIX) && n.ends_with(SUFFIX))
        })
        .collect();
    // The UTC timestamp right after the prefix makes name order age order.
    named.sort();
    let excess = named.len().saturating_sub(keep.max(1));
    let mut pruned = Vec::new();
    for path in named.into_iter().take(excess) {
        if path == just_written {
            continue;
        }
        std::fs::remove_file(&path).map_err(|e| io_error(&path, &e))?;
        pruned.push(path);
    }
    Ok(pruned)
}

#[cfg(test)]
mod tests {
    use super::default_name;

    #[test]
    fn default_names_sort_by_time() {
        let at = |s: &str| s.parse::<jiff::Timestamp>().unwrap();
        let a = default_name(at("2026-01-02T03:04:05Z"), None);
        let b = default_name(at("2026-01-02T03:04:06Z"), Some("before update!"));
        assert_eq!(a, "prometheus-20260102T030405Z.tar.zst");
        assert_eq!(b, "prometheus-20260102T030406Z-before-update-.tar.zst");
        assert!(a < b);
    }
}

//! The profile's snapshot store, `<profile>/snapshots/` (0700):
//!
//! - `objects/<aa>/<sha256>`: file contents, content-addressed and shared
//!   by every checkpoint and journal of the profile;
//! - `workspaces/<id>/meta.json` and `workspaces/<id>/checkpoints/*.json`:
//!   each project's checkpoints (manifests of its files);
//! - `journals/<session>.jsonl`: each session's file-tool edits.
//!
//! An object is written (temp file + rename) before any manifest or
//! journal line names it; unreferenced objects are removed by
//! [`SnapshotStore::collect_garbage`].

use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// One project's record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceMeta {
    /// The project's root directory.
    pub root: PathBuf,
    /// Last checkpoint or restore, Unix milliseconds.
    pub last_used_ms: i64,
}

/// The snapshot store of one profile.
#[derive(Debug, Clone)]
pub struct SnapshotStore {
    root: PathBuf,
}

/// Lower-case hex SHA-256 of `bytes`.
#[must_use]
pub fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut hex, b| {
            use std::fmt::Write as _;
            let _ = write!(hex, "{b:02x}");
            hex
        })
}

/// Unix milliseconds now.
#[must_use]
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

impl SnapshotStore {
    /// The store at `<profile_dir>/snapshots`.
    #[must_use]
    pub fn of_profile(profile_dir: &Path) -> Self {
        Self {
            root: profile_dir.join("snapshots"),
        }
    }

    /// The store's directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn dir(&self, rel: &str) -> io::Result<PathBuf> {
        let dir = self.root.join(rel);
        pmt_core::private_fs::create_dir(&self.root)?;
        fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    fn object_path(&self, hash: &str) -> PathBuf {
        self.root.join("objects").join(&hash[..2]).join(hash)
    }

    /// Stores `bytes` and returns their hash.
    ///
    /// # Errors
    ///
    /// The object cannot be written.
    pub fn put(&self, bytes: &[u8]) -> io::Result<String> {
        let hash = digest(bytes);
        let path = self.object_path(&hash);
        if path.exists() {
            return Ok(hash);
        }
        let dir = self.dir(&format!("objects/{}", &hash[..2]))?;
        let temp = dir.join(format!(".{hash}.{}", ulid::Ulid::generate()));
        let mut file = pmt_core::private_fs::new_file_options().open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temp, &path)?;
        Ok(hash)
    }

    /// The content of object `hash`.
    ///
    /// # Errors
    ///
    /// The object is missing or unreadable.
    pub fn get(&self, hash: &str) -> io::Result<Vec<u8>> {
        if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not an object hash",
            ));
        }
        fs::read(self.object_path(hash))
    }

    /// The id of the project rooted at `root`.
    #[must_use]
    pub fn workspace_id(root: &Path) -> String {
        digest(root.to_string_lossy().as_bytes())[..16].to_owned()
    }

    pub(crate) fn workspace_dir(&self, id: &str) -> PathBuf {
        self.root.join("workspaces").join(id)
    }

    /// Records that the project at `root` was used now.
    ///
    /// # Errors
    ///
    /// The record cannot be written.
    pub fn touch_workspace(&self, root: &Path) -> io::Result<String> {
        let id = Self::workspace_id(root);
        let dir = self.dir(&format!("workspaces/{id}/checkpoints"))?;
        let meta = WorkspaceMeta {
            root: root.to_path_buf(),
            last_used_ms: now_ms(),
        };
        write_json(&dir.with_file_name("meta.json"), &meta)?;
        Ok(id)
    }

    /// Every project with its id.
    #[must_use]
    pub fn workspaces(&self) -> Vec<(String, WorkspaceMeta)> {
        let Ok(entries) = fs::read_dir(self.root.join("workspaces")) else {
            return Vec::new();
        };
        let mut found: Vec<(String, WorkspaceMeta)> = entries
            .flatten()
            .filter_map(|entry| {
                let meta = read_json(&entry.path().join("meta.json"))?;
                Some((entry.file_name().to_string_lossy().into_owned(), meta))
            })
            .collect();
        found.sort_by_key(|w| std::cmp::Reverse(w.1.last_used_ms));
        found
    }

    /// The session journal file of `session`.
    pub(crate) fn journal_path(&self, session: &str) -> PathBuf {
        self.root.join("journals").join(format!("{session}.jsonl"))
    }

    /// Every session journal.
    pub(crate) fn journals(&self) -> Vec<PathBuf> {
        fs::read_dir(self.root.join("journals"))
            .map(|entries| entries.flatten().map(|e| e.path()).collect())
            .unwrap_or_default()
    }

    /// Bytes under the store.
    #[must_use]
    pub fn size(&self) -> u64 {
        dir_size(&self.root)
    }

    /// Removes objects no checkpoint or journal names.
    ///
    /// # Errors
    ///
    /// A directory cannot be read.
    pub fn collect_garbage(&self) -> io::Result<u64> {
        let mut referenced = std::collections::HashSet::new();
        for (id, _) in self.workspaces() {
            for checkpoint in super::checkpoint::list(self, &id) {
                referenced.extend(checkpoint.files.into_iter().filter_map(|f| f.hash));
            }
        }
        for journal in self.journals() {
            for edit in super::journal::read_edits(&journal) {
                referenced.extend(edit.before.into_iter().chain(edit.after));
            }
        }
        let mut freed = 0;
        let Ok(prefixes) = fs::read_dir(self.root.join("objects")) else {
            return Ok(0);
        };
        for prefix in prefixes.flatten() {
            for object in fs::read_dir(prefix.path())?.flatten() {
                let name = object.file_name().to_string_lossy().into_owned();
                if !referenced.contains(&name) {
                    freed += object.metadata().map_or(0, |m| m.len());
                    let _ = fs::remove_file(object.path());
                }
            }
        }
        Ok(freed)
    }

    /// Deletes the whole store.
    ///
    /// # Errors
    ///
    /// It cannot be removed.
    pub fn clear(&self) -> io::Result<()> {
        match fs::remove_dir_all(&self.root) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => Err(err),
            _ => Ok(()),
        }
    }
}

/// Bytes of every file under `dir`.
pub(crate) fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.file_type() {
            Ok(kind) if kind.is_dir() => dir_size(&entry.path()),
            Ok(_) => entry.metadata().map_or(0, |m| m.len()),
            Err(_) => 0,
        })
        .sum()
}

pub(crate) fn write_json<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    let temp = path.with_extension(format!("tmp-{}", ulid::Ulid::generate()));
    fs::write(&temp, bytes)?;
    fs::rename(temp, path)
}

pub(crate) fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

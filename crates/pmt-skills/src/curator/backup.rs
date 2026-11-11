//! Backups of the skills tree (`skills/.backups/curator-<UTC stamp>.tar.zst`):
//! written before every real curator pass and by `prometheus curator
//! backup`, rotated to the newest `curator.backup_keep`, and restored
//! whole by `prometheus curator rollback` without an entry id.

use std::path::{Path, PathBuf};

use jiff::Timestamp;
use pmt_core::error::{Error, codes};
use pmt_core::private_fs;

/// The backup directory below `skills/`.
pub const BACKUP_DIR: &str = ".backups";
const PREFIX: &str = "curator-";
const SUFFIX: &str = ".tar.zst";

fn io(path: &Path, e: &std::io::Error) -> Error {
    Error::io(&codes::SKILL_IO, path, e)
}

fn failed(why: impl std::fmt::Display) -> Error {
    Error::new(&codes::SKILL_IO, format!("skills backup: {why}"))
}

/// The files a backup holds: every file below `skills/` except the
/// backups and locks themselves.
fn files(skills_dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    collect(skills_dir, skills_dir, &mut out);
    out.sort();
    out
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() && !skipped(root, dir, &entry.file_name()) {
            collect(root, &path, out);
        } else if kind.is_file() {
            out.extend(
                path.strip_prefix(root)
                    .ok()
                    .map(|rel| rel.to_string_lossy().replace('\\', "/")),
            );
        }
    }
}

/// The backups and locks of the top level are never backed up.
fn skipped(root: &Path, dir: &Path, name: &std::ffi::OsStr) -> bool {
    dir == root && (name == BACKUP_DIR || name == ".locks")
}

/// Every backup, oldest first.
#[must_use]
pub fn list(skills_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(skills_dir.join(BACKUP_DIR)) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(PREFIX) && n.ends_with(SUFFIX))
        })
        .collect();
    out.sort();
    out
}

/// Writes a backup of the tree and keeps the newest `keep`; its path.
///
/// # Errors
///
/// `skill.io`.
pub fn write(skills_dir: &Path, keep: usize, now: Timestamp) -> Result<PathBuf, Error> {
    let dir = skills_dir.join(BACKUP_DIR);
    private_fs::create_dir(&dir).map_err(|e| io(&dir, &e))?;
    let mut path = dir.join(format!(
        "{PREFIX}{}{SUFFIX}",
        now.strftime("%Y%m%dT%H%M%S%.3fZ")
    ));
    let mut n = 1;
    while path.exists() {
        path = dir.join(format!(
            "{PREFIX}{}-{n}{SUFFIX}",
            now.strftime("%Y%m%dT%H%M%S%.3fZ")
        ));
        n += 1;
    }
    let mut builder = tar::Builder::new(Vec::new());
    for rel in files(skills_dir) {
        let bytes =
            std::fs::read(skills_dir.join(&rel)).map_err(|e| io(&skills_dir.join(&rel), &e))?;
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o600);
        header.set_cksum();
        builder
            .append_data(&mut header, &rel, bytes.as_slice())
            .map_err(failed)?;
    }
    let tarball = builder.into_inner().map_err(failed)?;
    let packed = zstd::encode_all(tarball.as_slice(), 3).map_err(failed)?;
    private_fs::write_atomic(&path, &packed).map_err(|e| io(&path, &e))?;
    let all = list(skills_dir);
    for old in all.iter().take(all.len().saturating_sub(keep.max(1))) {
        let _ = std::fs::remove_file(old);
    }
    Ok(path)
}

/// Replaces the tree (except backups and locks) with the content of
/// `backup`.
///
/// # Errors
///
/// `skill.io` for an unreadable backup or a write failure.
pub fn restore(skills_dir: &Path, backup: &Path) -> Result<(), Error> {
    let packed = std::fs::read(backup).map_err(|e| io(backup, &e))?;
    let tarball = zstd::decode_all(packed.as_slice()).map_err(failed)?;
    let mut entries = Vec::new();
    let mut archive = tar::Archive::new(tarball.as_slice());
    for entry in archive.entries().map_err(failed)? {
        let mut entry = entry.map_err(failed)?;
        let rel = entry
            .path()
            .map_err(failed)?
            .to_string_lossy()
            .replace('\\', "/");
        if rel.split('/').any(|p| p == ".." || p.is_empty()) || rel.starts_with('/') {
            return Err(failed(format!(
                "unsafe path '{rel}' in {}",
                backup.display()
            )));
        }
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut bytes).map_err(failed)?;
        entries.push((rel, bytes));
    }
    for rel in files(skills_dir) {
        let _ = std::fs::remove_file(skills_dir.join(rel));
    }
    for (rel, bytes) in entries {
        let path = rel
            .split('/')
            .fold(skills_dir.to_path_buf(), |p, c| p.join(c));
        if let Some(parent) = path.parent() {
            private_fs::create_dir(parent).map_err(|e| io(parent, &e))?;
        }
        private_fs::write_atomic(&path, &bytes).map_err(|e| io(&path, &e))?;
    }
    prune_empty(skills_dir, skills_dir);
    Ok(())
}

fn prune_empty(root: &Path, dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            prune_empty(root, &entry.path());
        }
    }
    if dir != root {
        let _ = std::fs::remove_dir(dir);
    }
}

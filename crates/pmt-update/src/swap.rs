//! Atomic replacement of the installed binary (spec: distribution-and-update,
//! Atomic binary swap with health check; Rollback).
//!
//! The new bytes are first written and synced to a temporary file in the
//! binary's own directory, then renamed over the binary: a rename within one
//! directory is atomic on POSIX, so after a crash or power loss the path
//! holds the complete old or the complete new file, never a mix. Windows
//! cannot replace a running executable, so its file is renamed aside first
//! (`<binary>.old`, removed on the next swap); the gap between the two
//! renames is the one window in which the path is briefly empty.

use std::path::{Path, PathBuf};

use pmt_core::error::Error;

use crate::archive::write_executable;
use crate::install::io_error;

/// `prometheus.prev` (Windows: `prometheus.prev.exe`) beside `binary`.
#[must_use]
pub fn prev_path(binary: &Path) -> PathBuf {
    let name = if cfg!(windows) {
        "prometheus.prev.exe"
    } else {
        "prometheus.prev"
    };
    binary.with_file_name(name)
}

/// Installs `staged` at `binary`, keeping the current binary as
/// [`prev_path`].
///
/// # Errors
///
/// `update.failed`; the installed binary is unchanged on failure.
pub fn install(staged: &Path, binary: &Path) -> Result<(), Error> {
    let current = std::fs::read(binary).map_err(|e| io_error(binary, &e))?;
    replace(&prev_path(binary), &current)?;
    let new = std::fs::read(staged).map_err(|e| io_error(staged, &e))?;
    replace(binary, &new)
}

/// Puts [`prev_path`] back at `binary` (the previous binary stays, so a
/// second rollback is harmless).
///
/// # Errors
///
/// `update.failed`; the installed binary is unchanged on failure.
pub fn restore_prev(binary: &Path) -> Result<(), Error> {
    let prev = prev_path(binary);
    let bytes = std::fs::read(&prev).map_err(|e| io_error(&prev, &e))?;
    replace(binary, &bytes)
}

/// Atomically makes `path` hold `bytes` (mode 0755).
fn replace(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".new.{}", std::process::id()));
    let temp = path.with_file_name(name);
    let written = write_executable(&temp, bytes).and_then(|()| rename_over(&temp, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    written?;
    sync_dir(path);
    Ok(())
}

#[cfg(not(windows))]
fn rename_over(temp: &Path, path: &Path) -> Result<(), Error> {
    std::fs::rename(temp, path).map_err(|e| io_error(path, &e))
}

#[cfg(windows)]
fn rename_over(temp: &Path, path: &Path) -> Result<(), Error> {
    let mut aside = path.as_os_str().to_os_string();
    aside.push(".old");
    let aside = PathBuf::from(aside);
    // Left by the previous swap; deletable now that process has exited.
    let _ = std::fs::remove_file(&aside);
    let moved_aside = path.exists();
    if moved_aside {
        std::fs::rename(path, &aside).map_err(|e| io_error(path, &e))?;
    }
    std::fs::rename(temp, path).map_err(|e| {
        if moved_aside {
            let _ = std::fs::rename(&aside, path);
        }
        io_error(path, &e)
    })
}

/// Persists the rename itself (POSIX directories are synced separately).
fn sync_dir(path: &Path) {
    #[cfg(unix)]
    if let Some(dir) = path.parent() {
        let _ = std::fs::File::open(dir).and_then(|d| d.sync_all());
    }
    #[cfg(not(unix))]
    let _ = path;
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::{install, prev_path, restore_prev};

    #[test]
    fn swap_keeps_the_previous_binary_and_never_rewrites_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("prometheus");
        let staged = dir.path().join("staged");
        std::fs::write(&binary, b"old build").unwrap();
        std::fs::write(&staged, b"new build").unwrap();
        // A reader that opened the old binary (a running process) keeps
        // seeing the complete old file: the path was renamed over, not
        // overwritten.
        let mut open_old = std::fs::File::open(&binary).unwrap();
        install(&staged, &binary).unwrap();
        let mut seen = Vec::new();
        open_old.read_to_end(&mut seen).unwrap();
        assert_eq!(seen, b"old build");
        assert_eq!(std::fs::read(&binary).unwrap(), b"new build");
        assert_eq!(std::fs::read(prev_path(&binary)).unwrap(), b"old build");
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".new."))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn rollback_restores_the_previous_binary() {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("prometheus");
        let staged = dir.path().join("staged");
        std::fs::write(&binary, b"old build").unwrap();
        std::fs::write(&staged, b"new build").unwrap();
        install(&staged, &binary).unwrap();
        restore_prev(&binary).unwrap();
        assert_eq!(std::fs::read(&binary).unwrap(), b"old build");
    }

    #[test]
    fn rollback_without_previous_fails_and_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("prometheus");
        std::fs::write(&binary, b"build").unwrap();
        assert!(restore_prev(&binary).is_err());
        assert_eq!(std::fs::read(&binary).unwrap(), b"build");
    }
}

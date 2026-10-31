//! The single-writer update lock (spec: distribution-and-update,
//! Single-writer update lock): an exclusive OS lock on
//! `<home>/updates/update.lock`, taken before any mutation and never waited
//! for. The holder (pid, start, binary) is recorded in `update.holder.json`
//! because Windows forbids reading a locked file.

use std::fs::{File, TryLockError};
use std::path::{Path, PathBuf};

use pmt_core::error::{Error, codes};
use pmt_core::private_fs;
use serde::{Deserialize, Serialize};

use crate::install::{create_dirs, io_error};

/// The lock file name in `<home>/updates/`.
pub const LOCK_FILE: &str = "update.lock";
const HOLDER_FILE: &str = "update.holder.json";

/// Who holds the lock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Holder {
    /// Process id.
    pub pid: u32,
    /// RFC 3339 time the update started.
    pub started_at: String,
    /// The binary being updated.
    pub binary: PathBuf,
}

/// The held lock; released on drop.
#[derive(Debug)]
pub struct UpdateLock {
    _file: File,
    holder_path: PathBuf,
}

impl UpdateLock {
    /// Takes the lock for updating `binary`.
    ///
    /// # Errors
    ///
    /// `update.locked` (exit 75) `another update is running (pid <pid>,
    /// started <time>)` when another process holds it; `update.failed` on
    /// I/O errors.
    ///
    /// # Panics
    ///
    /// Never: the holder record always serializes.
    pub fn acquire(home: &Path, binary: &Path) -> Result<Self, Error> {
        let dir = home.join("updates");
        create_dirs(&dir)?;
        let path = dir.join(LOCK_FILE);
        let file = match private_fs::new_file_options().open(&path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => File::options()
                .write(true)
                .open(&path)
                .map_err(|e| io_error(&path, &e))?,
            Err(e) => return Err(io_error(&path, &e)),
        };
        let holder_path = dir.join(HOLDER_FILE);
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(held(&holder_path)),
            Err(TryLockError::Error(e)) => return Err(io_error(&path, &e)),
        }
        let holder = Holder {
            pid: std::process::id(),
            started_at: jiff::Timestamp::now().to_string(),
            binary: binary.to_owned(),
        };
        let json = serde_json::to_vec(&holder).expect("holder serializes");
        private_fs::write_atomic(&holder_path, &json).map_err(|e| io_error(&holder_path, &e))?;
        Ok(Self {
            _file: file,
            holder_path,
        })
    }
}

impl Drop for UpdateLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.holder_path);
    }
}

fn held(holder_path: &Path) -> Error {
    let holder: Option<Holder> = std::fs::read(holder_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok());
    let who = holder.map_or_else(
        || "pid unknown".to_owned(),
        |h| format!("pid {}, started {}", h.pid, h.started_at),
    );
    Error::new(
        &codes::UPDATE_LOCKED,
        format!("another update is running ({who})"),
    )
    .with_hint("wait for it to finish; nothing was changed")
}

#[cfg(test)]
mod tests {
    use super::UpdateLock;

    #[test]
    fn second_holder_is_refused_with_the_first_ones_pid() {
        let home = tempfile::tempdir().unwrap();
        let first = UpdateLock::acquire(home.path(), "/bin/p".as_ref()).unwrap();
        let err = UpdateLock::acquire(home.path(), "/bin/p".as_ref()).unwrap_err();
        assert_eq!(err.exit_code(), 75);
        let pid = std::process::id();
        assert!(
            err.message()
                .starts_with(&format!("another update is running (pid {pid}, started ")),
            "{err}"
        );
        drop(first);
        drop(UpdateLock::acquire(home.path(), "/bin/p".as_ref()).unwrap());
    }
}

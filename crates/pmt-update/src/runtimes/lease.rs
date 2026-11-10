//! Runtime leases (spec: distribution-and-update, Runtime leases and
//! garbage collection): a process using a runtime version holds a lease on
//! it, and `prometheus runtimes gc` and `remove` keep every leased version.
//!
//! Each holder creates its own file under
//! `<store>/<name>/.leases/<version dir>/` and holds an exclusive OS lock on
//! it for as long as it uses the runtime; together the holders form a
//! shared lease on the version. The OS releases the lock when the process
//! exits, however it exits, so a crashed holder never pins a version: its
//! leftover file is unlocked and is swept by the next count.

use pmt_core::error::codes;
use std::fs::File;
use std::path::{Path, PathBuf};

use pmt_core::error::Error;

const LEASES: &str = ".leases";

/// A held lease; released on drop.
#[derive(Debug)]
pub struct Lease {
    file: Option<File>,
    path: PathBuf,
}

impl Lease {
    /// Leases version `dir_name` of runtime `name` in the store at `root`.
    ///
    /// # Errors
    ///
    /// `runtime.io` when the lease file cannot be created or locked.
    pub fn acquire(root: &Path, name: &str, dir_name: &str) -> Result<Self, Error> {
        let dir = lease_dir(root, name, dir_name);
        std::fs::create_dir_all(&dir).map_err(|e| Error::io(&codes::RUNTIME_IO, &dir, &e))?;
        let path = dir.join(format!(
            "{}-{}.lease",
            std::process::id(),
            ulid::Ulid::generate()
        ));
        let file = File::create(&path).map_err(|e| Error::io(&codes::RUNTIME_IO, &path, &e))?;
        pmt_core::file_lock::lock(&file).map_err(|e| Error::io(&codes::RUNTIME_IO, &path, &e))?;
        Ok(Self {
            file: Some(file),
            path,
        })
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        // Closed before removal: Windows refuses to delete an open file.
        drop(self.file.take());
        let _ = std::fs::remove_file(&self.path);
    }
}

/// How many processes lease version `dir_name` of `name`; lease files left
/// by processes that are gone are removed.
#[must_use]
pub fn users(root: &Path, name: &str, dir_name: &str) -> usize {
    let Ok(entries) = std::fs::read_dir(lease_dir(root, name, dir_name)) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|entry| {
            let path = entry.path();
            let held = is_held(&path);
            if !held {
                let _ = std::fs::remove_file(&path);
            }
            held
        })
        .count()
}

/// Whether another process (or another handle of this one) holds the lock
/// on `path`.
fn is_held(path: &Path) -> bool {
    let Ok(file) = File::options().read(true).write(true).open(path) else {
        // An existing file that cannot be opened (a sharing violation on
        // Windows) is in use: count it as held rather than collect it.
        return path.exists();
    };
    // A lock that cannot even be tried is treated as held: never collect
    // what might be in use.
    !matches!(pmt_core::file_lock::try_lock(&file), Ok(()))
}

/// Removes the lease directory of a version that no longer exists.
pub(crate) fn forget(root: &Path, name: &str, dir_name: &str) {
    let _ = std::fs::remove_dir_all(lease_dir(root, name, dir_name));
}

fn lease_dir(root: &Path, name: &str, dir_name: &str) -> PathBuf {
    root.join(name).join(LEASES).join(dir_name)
}

#[cfg(test)]
mod tests {
    use super::{Lease, users};

    #[test]
    fn a_lease_counts_until_dropped_and_leaves_no_file() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(users(dir.path(), "node", "1-a"), 0);
        let lease = Lease::acquire(dir.path(), "node", "1-a").unwrap();
        // Same process, separate handle: the OS lock still conflicts.
        assert_eq!(users(dir.path(), "node", "1-a"), 1);
        drop(lease);
        assert_eq!(users(dir.path(), "node", "1-a"), 0);
    }

    #[test]
    fn an_unlocked_leftover_file_is_not_a_user() {
        let dir = tempfile::tempdir().unwrap();
        let leases = dir.path().join("node/.leases/1-a");
        std::fs::create_dir_all(&leases).unwrap();
        std::fs::write(leases.join("1-x.lease"), b"").unwrap();
        assert_eq!(users(dir.path(), "node", "1-a"), 0);
        assert!(!leases.join("1-x.lease").exists());
    }
}

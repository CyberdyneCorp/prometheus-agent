//! Advisory exclusive file locks that work on every target. std's
//! file locks are unsupported on Android (`try_lock()` returns "not
//! supported"), which made every home writer (`chat`, the daemon, backups,
//! updates, downloads, secret writes) fail on Termux; there the same `flock(2)` std uses
//! on Linux is called directly. Like std's, the lock is released when the
//! file is closed or the process dies.

use std::fs::{File, TryLockError};

/// Takes an exclusive lock on `file` without blocking.
///
/// # Errors
///
/// `WouldBlock` when another handle holds a lock on the file, else the
/// OS error.
pub fn try_lock(file: &File) -> Result<(), TryLockError> {
    imp::try_lock(file)
}

/// Takes an exclusive lock on `file`, waiting for any holder.
///
/// # Errors
///
/// The OS error.
pub fn lock(file: &File) -> std::io::Result<()> {
    imp::lock(file)
}

#[cfg(not(target_os = "android"))]
mod imp {
    use std::fs::{File, TryLockError};

    // The sanctioned calls of std's locks.
    #[allow(clippy::disallowed_methods)]
    pub(super) fn try_lock(file: &File) -> Result<(), TryLockError> {
        file.try_lock()
    }

    #[allow(clippy::disallowed_methods)]
    pub(super) fn lock(file: &File) -> std::io::Result<()> {
        file.lock()
    }
}

#[cfg(target_os = "android")]
mod imp {
    use std::fs::{File, TryLockError};

    use rustix::fs::{FlockOperation, flock};
    use rustix::io::Errno;

    pub(super) fn try_lock(file: &File) -> Result<(), TryLockError> {
        match flock(file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => Ok(()),
            Err(Errno::WOULDBLOCK) => Err(TryLockError::WouldBlock),
            Err(errno) => Err(TryLockError::Error(errno.into())),
        }
    }

    pub(super) fn lock(file: &File) -> std::io::Result<()> {
        Ok(flock(file, FlockOperation::LockExclusive)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A second handle on a locked file is refused with `WouldBlock`, and
    /// closing the holder releases the lock.
    #[test]
    fn a_held_lock_refuses_a_second_handle_until_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lock");
        let open = || {
            File::options()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&path)
                .unwrap()
        };
        let holder = open();
        try_lock(&holder).unwrap();
        let other = open();
        assert!(matches!(try_lock(&other), Err(TryLockError::WouldBlock)));
        drop(holder);
        try_lock(&other).unwrap();
        drop(other);
        lock(&open()).unwrap();
    }
}

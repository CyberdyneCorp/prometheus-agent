//! Platform credential locks across homes (spec: platform-adapters,
//! Credential locks across homes): an adapter connecting with a unique
//! credential (a bot token) first takes a machine-global lock file keyed
//! by platform and the SHA-256 of the credential identity, in the OS
//! user's runtime directory. A live holder elsewhere (another home, or
//! another profile of this daemon) keeps it: the adapter is `fatal` with
//! `<platform>_lock` (retryable) naming the holder. Only the holding
//! process releases a lock; `daemon run --replace` may take over a live
//! holder, at most once per connect.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use pmt_core::runtime::{Liveness, process_start_time};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::adapter::AdapterError;

/// Who holds a lock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Holder {
    pub(crate) home: PathBuf,
    pub(crate) profile: String,
    pub(crate) pid: u32,
    pub(crate) start_time: u64,
}

impl Holder {
    /// This process, serving `profile` of `home`.
    pub(crate) fn current(home: &Path, profile: &str) -> Self {
        let pid = std::process::id();
        Self {
            home: home.to_path_buf(),
            profile: profile.to_owned(),
            pid,
            start_time: process_start_time(pid).unwrap_or(0),
        }
    }

    fn liveness(&self) -> Liveness {
        match process_start_time(self.pid) {
            None => Liveness::Dead,
            Some(started) if started == self.start_time => Liveness::Alive,
            Some(_) => Liveness::Reused,
        }
    }
}

/// A held lock; dropping it releases the file when it still names the
/// holder.
#[derive(Debug)]
pub(crate) struct CredentialLock {
    path: PathBuf,
    holder: Holder,
}

impl Drop for CredentialLock {
    fn drop(&mut self) {
        if read(&self.path).as_ref() == Some(&self.holder) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn read(path: &Path) -> Option<Holder> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// The lock file of `identity` on `platform`.
fn lock_path(dir: &Path, platform: &str, identity: &str) -> PathBuf {
    let digest = crate::tickets::hex(&Sha256::digest(identity.as_bytes()));
    dir.join(format!("{platform}-{digest}.lock"))
}

fn create(path: &Path, holder: &Holder) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(serde_json::to_string(holder)?.as_bytes())
}

/// Takes the lock of `identity` on `platform` for `holder`. A stale file
/// (its process gone) is replaced; a live foreign holder is taken over
/// only with `replace`.
pub(crate) fn acquire(
    dir: &Path,
    platform: &str,
    identity: &str,
    holder: Holder,
    replace: bool,
) -> Result<CredentialLock, AdapterError> {
    let io = |e: std::io::Error| AdapterError {
        retryable: true,
        code: format!("{platform}_lock"),
        message: format!("the credential lock in {} is unusable: {e}", dir.display()),
    };
    std::fs::create_dir_all(dir).map_err(io)?;
    let path = lock_path(dir, platform, identity);
    for _ in 0..3 {
        match create(&path, &holder) {
            Ok(()) => return Ok(CredentialLock { path, holder }),
            Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => return Err(io(e)),
            Err(_) => {}
        }
        let Some(current) = read(&path) else {
            // Being written or corrupt: a holder that crashed mid-write.
            let _ = std::fs::remove_file(&path);
            continue;
        };
        if current.liveness() != Liveness::Alive {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        if !replace {
            return Err(held(platform, &current));
        }
        tracing::warn!(
            platform,
            "taking over the credential lock from profile '{}' of {} (pid {})",
            current.profile,
            current.home.display(),
            current.pid
        );
        std::fs::write(
            &path,
            serde_json::to_string(&holder).map_err(|e| io(e.into()))?,
        )
        .map_err(io)?;
        return Ok(CredentialLock { path, holder });
    }
    Err(io(std::io::Error::other("the lock file keeps changing")))
}

/// The error of a credential held by a live process.
fn held(platform: &str, holder: &Holder) -> AdapterError {
    AdapterError {
        retryable: true,
        code: format!("{platform}_lock"),
        message: format!(
            "this {platform} credential is in use by profile '{}' of {} (pid {}); stop it with `prometheus -p {} gateway stop`, or run `prometheus daemon run --replace` to take it over",
            holder.profile,
            holder.home.display(),
            holder.pid,
            holder.profile
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_live_holder_keeps_the_lock_and_a_dead_one_loses_it() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let first = acquire(
            dir.path(),
            "telegram",
            "123:ABC",
            Holder::current(&home, "default"),
            false,
        )
        .unwrap();
        let err = acquire(
            dir.path(),
            "telegram",
            "123:ABC",
            Holder::current(&home, "work"),
            false,
        )
        .unwrap_err();
        assert_eq!(err.code, "telegram_lock");
        assert!(err.retryable);
        assert!(err.message.contains("profile 'default'"), "{}", err.message);
        assert!(err.message.contains("prometheus -p default gateway stop"));
        drop(first);
        let dead = Holder {
            pid: u32::MAX - 7,
            ..Holder::current(&home, "old")
        };
        std::fs::write(
            lock_path(dir.path(), "telegram", "123:ABC"),
            serde_json::to_string(&dead).unwrap(),
        )
        .unwrap();
        acquire(
            dir.path(),
            "telegram",
            "123:ABC",
            Holder::current(&home, "work"),
            false,
        )
        .unwrap();
    }

    #[test]
    fn replace_takes_over_and_the_old_holder_cannot_release_it() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let old = acquire(
            dir.path(),
            "discord",
            "t",
            Holder::current(&home, "a"),
            false,
        )
        .unwrap();
        let new = acquire(
            dir.path(),
            "discord",
            "t",
            Holder::current(&home, "b"),
            true,
        )
        .unwrap();
        drop(old);
        let path = lock_path(dir.path(), "discord", "t");
        assert_eq!(read(&path).unwrap().profile, "b");
        drop(new);
        assert!(!path.exists());
    }
}

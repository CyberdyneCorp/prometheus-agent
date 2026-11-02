//! The machine-global daemon registry (spec: messaging-gateway, Daemon
//! inventory across homes): every running daemon of the OS user, whatever
//! its home, records itself in the user's runtime directory, one file per
//! home. Liveness is the pid plus its OS start time (never argv), so a
//! crashed daemon's entry reads as stale and is pruned by the next reader.
//!
//! The directory is per OS user, outside every home: `~/.local/state/
//! prometheus/daemons` on Unix and `%LOCALAPPDATA%\Prometheus\daemons` on
//! Windows. Platform credential locks (platform-adapters) keep their lock
//! files beside it, under [`user_runtime_dir`].

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::env::LaunchEnv;
use crate::error::{Error, codes};
use crate::private_fs;
use crate::runtime::{Liveness, process_start_time};

/// The OS user's machine-global runtime directory, `None` when the user's
/// home is unknown.
#[must_use]
pub fn user_runtime_dir(launch: &LaunchEnv) -> Option<PathBuf> {
    if cfg!(windows) {
        launch
            .local_app_data()
            .map(|dir| dir.join("Prometheus"))
            .or_else(|| {
                launch
                    .user_home()
                    .map(|h| h.join("AppData/Local/Prometheus"))
            })
    } else {
        launch
            .user_home()
            .map(|home| home.join(".local/state/prometheus"))
    }
}

/// Where the registry's entries live.
#[must_use]
pub fn registry_dir(launch: &LaunchEnv) -> Option<PathBuf> {
    user_runtime_dir(launch).map(|dir| dir.join("daemons"))
}

/// One daemon's entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonEntry {
    /// Its home.
    pub home: PathBuf,
    /// OS process id.
    pub pid: u32,
    /// OS-reported start time of `pid` (Unix seconds).
    pub start_time: u64,
    /// The binary's version.
    pub version: String,
    /// The binary's build id.
    pub build_id: String,
    /// Served profiles.
    pub profiles: Vec<String>,
    /// Enabled messaging adapters by profile.
    pub adapters: BTreeMap<String, Vec<String>>,
}

impl DaemonEntry {
    /// The calling process's entry for `home`.
    #[must_use]
    pub fn current(home: &Path, build_id: &str, profiles: Vec<String>) -> Self {
        let pid = std::process::id();
        Self {
            home: home.to_path_buf(),
            pid,
            start_time: process_start_time(pid).unwrap_or(0),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            build_id: build_id.to_owned(),
            profiles,
            adapters: BTreeMap::new(),
        }
    }

    /// Checks the pid and its start time against the OS.
    #[must_use]
    pub fn liveness(&self) -> Liveness {
        match process_start_time(self.pid) {
            None => Liveness::Dead,
            Some(started) if started == self.start_time => Liveness::Alive,
            Some(_) => Liveness::Reused,
        }
    }
}

/// One listed entry and whether its process still runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    /// The entry as recorded.
    pub entry: DaemonEntry,
    /// `false`: the pid is gone or was reused (the entry was pruned).
    pub alive: bool,
}

/// The entry file of `home` (named by a hash of its path, so two homes
/// never share one and the name holds no path text).
fn entry_file(dir: &Path, home: &Path) -> PathBuf {
    let digest = Sha256::digest(home.to_string_lossy().as_bytes());
    let name = digest[..8].iter().fold(String::new(), |mut name, b| {
        let _ = write!(name, "{b:02x}");
        name
    });
    dir.join(format!("{name}.json"))
}

/// Records (or rewrites) `entry` in the registry under `dir`.
///
/// # Errors
///
/// `home.io` when the directory or file cannot be written.
pub fn register(dir: &Path, entry: &DaemonEntry) -> Result<(), Error> {
    private_fs::create_dir(dir).map_err(|e| Error::io(&codes::HOME_IO, dir, &e))?;
    let file = entry_file(dir, &entry.home);
    let text = serde_json::to_string_pretty(entry).map_err(|e| {
        Error::new(
            &codes::HOME_IO,
            format!("cannot encode the daemon entry: {e}"),
        )
    })?;
    private_fs::write_atomic(&file, text.as_bytes())
        .map_err(|e| Error::io(&codes::HOME_IO, &file, &e))
}

/// Removes `home`'s entry when it is the calling process's (a newer daemon
/// of the same home may have replaced it).
pub fn unregister(dir: &Path, home: &Path) {
    let file = entry_file(dir, home);
    let mine = std::fs::read(&file)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<DaemonEntry>(&bytes).ok())
        .is_some_and(|entry| entry.pid == std::process::id());
    if mine {
        let _ = std::fs::remove_file(file);
    }
}

/// Every entry under `dir`, sorted by home; stale entries are reported
/// (`alive = false`) and removed. A missing directory is an empty list.
///
/// # Errors
///
/// `home.io` when the directory exists but cannot be read.
pub fn list(dir: &Path) -> Result<Vec<Listed>, Error> {
    let reader = match std::fs::read_dir(dir) {
        Ok(reader) => reader,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(Error::io(&codes::HOME_IO, dir, &e)),
    };
    let mut listed = Vec::new();
    for item in reader.flatten() {
        let path = item.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        // Another build's or a half-written file: not ours to judge.
        let Some(entry) = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<DaemonEntry>(&bytes).ok())
        else {
            continue;
        };
        let alive = entry.liveness() == Liveness::Alive;
        if !alive {
            let _ = std::fs::remove_file(&path);
        }
        listed.push(Listed { entry, alive });
    }
    listed.sort_by(|a, b| a.entry.home.cmp(&b.entry.home));
    Ok(listed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_homes_are_two_entries_and_a_dead_one_is_pruned() {
        let dir = tempfile::tempdir().unwrap();
        let mut first = DaemonEntry::current(Path::new("/srv/pmt-work"), "b1", vec!["work".into()]);
        first
            .adapters
            .insert("work".into(), vec!["telegram".into()]);
        register(dir.path(), &first).unwrap();
        let second = DaemonEntry::current(
            Path::new("/home/a/.prometheus"),
            "b1",
            vec!["default".into()],
        );
        register(dir.path(), &second).unwrap();
        let mut dead = DaemonEntry::current(Path::new("/tmp/gone"), "b0", Vec::new());
        dead.start_time = dead.start_time.wrapping_add(12_345);
        register(dir.path(), &dead).unwrap();

        let listed = list(dir.path()).unwrap();
        let homes: Vec<_> = listed
            .iter()
            .map(|l| (l.entry.home.clone(), l.alive))
            .collect();
        assert_eq!(
            homes,
            vec![
                (PathBuf::from("/home/a/.prometheus"), true),
                (PathBuf::from("/srv/pmt-work"), true),
                (PathBuf::from("/tmp/gone"), false),
            ]
        );
        assert_eq!(
            list(dir.path()).unwrap().len(),
            2,
            "the stale entry is pruned"
        );
        unregister(dir.path(), Path::new("/srv/pmt-work"));
        assert_eq!(list(dir.path()).unwrap().len(), 1);
    }

    #[test]
    fn no_registry_is_an_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        assert!(list(&dir.path().join("missing")).unwrap().is_empty());
    }
}

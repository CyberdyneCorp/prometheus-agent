//! Runtime records under `<home>/run/` and the home's single-writer lock
//! (spec: cli, Process identity from runtime records; workspace-architecture,
//! Single-writer storage invariant).
//!
//! A process is identified by its pid plus the OS-reported start time of
//! that pid, never by scanning command lines: a record whose pid is gone, or
//! whose pid was reused by a process with a different start time, is stale.
//!
//! The writer lock is an exclusive OS lock on `run/gateway.lock`. The daemon
//! holds it while it runs; without a daemon an in-process `prometheus chat`
//! holds it for its lifetime. The OS releases it when the holder dies, so a
//! crash never leaves the home locked; only the JSON record can go stale.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sysinfo::{Pid, ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System};

use crate::error::{Error, codes};
use crate::private_fs;

/// The lock file every writer takes (spec: One daemon per Prometheus home).
pub const LOCK_FILE: &str = "gateway.lock";

/// What kind of process wrote a record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// The gateway daemon; its record is `run/gateway.json`.
    Daemon,
    /// An in-process `prometheus chat` with no daemon; `run/chat.json`.
    Chat,
}

impl Role {
    /// Every role, for sweeping records.
    pub const ALL: [Self; 2] = [Self::Daemon, Self::Chat];

    /// File name of this role's record in `run/`.
    #[must_use]
    pub const fn record_file(self) -> &'static str {
        match self {
            Self::Daemon => "gateway.json",
            Self::Chat => "chat.json",
        }
    }
}

/// One process's runtime record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeRecord {
    /// OS process id.
    pub pid: u32,
    /// OS-reported start time of `pid`, in seconds since the Unix epoch.
    pub start_time: u64,
    /// The writer's semantic version.
    pub version: String,
    /// What kind of process this is.
    pub role: Role,
    /// Profiles the process serves.
    pub profiles: Vec<String>,
}

/// Whether a record still describes a running process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    /// The pid runs and started at the recorded time.
    Alive,
    /// No process has the pid.
    Dead,
    /// The pid now belongs to a different process (other start time).
    Reused,
}

impl RuntimeRecord {
    /// The record of the calling process.
    #[must_use]
    pub fn current(role: Role, profiles: Vec<String>) -> Self {
        let pid = std::process::id();
        Self {
            pid,
            // The current process always exists; 0 would only make the
            // record look stale to readers, never alive by mistake.
            start_time: process_start_time(pid).unwrap_or(0),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            role,
            profiles,
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

/// Start time of a running (not zombie) process, in Unix seconds.
fn process_start_time(pid: u32) -> Option<u64> {
    let pid = Pid::from_u32(pid);
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing(),
    );
    system
        .process(pid)
        .filter(|process| process.status() != ProcessStatus::Zombie)
        .map(sysinfo::Process::start_time)
}

/// `<home>/run`.
#[must_use]
pub fn run_dir(home: &Path) -> PathBuf {
    home.join("run")
}

/// Reads a role's record as written, stale or not. A missing or unreadable
/// record is `None`: an unparsable file may belong to another build, so it
/// is reported as absent and left in place.
///
/// # Errors
///
/// `home.io` when the file exists but cannot be read.
pub fn read_record(home: &Path, role: Role) -> Result<Option<RuntimeRecord>, Error> {
    let path = run_dir(home).join(role.record_file());
    match fs::read(&path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes).ok()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_error(&path, e)),
    }
}

/// The role's record when its process is alive. A stale record (dead pid or
/// reused pid) is removed and reported as `None`.
///
/// # Errors
///
/// `home.io` when the record cannot be read or the stale one removed.
pub fn live_record(home: &Path, role: Role) -> Result<Option<RuntimeRecord>, Error> {
    let Some(record) = read_record(home, role)? else {
        return Ok(None);
    };
    if record.liveness() == Liveness::Alive {
        return Ok(Some(record));
    }
    remove_record(home, role)?;
    Ok(None)
}

fn remove_record(home: &Path, role: Role) -> Result<(), Error> {
    let path = run_dir(home).join(role.record_file());
    match fs::remove_file(&path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(io_error(&path, e)),
        _ => Ok(()),
    }
}

/// The home's exclusive writer lock, released when dropped (or when the
/// process dies). While held, its record is `run/<role>.json`.
#[derive(Debug)]
pub struct WriterLock {
    // Held only for its OS lock; closing the file releases it.
    _file: File,
    home: PathBuf,
    record: RuntimeRecord,
}

impl WriterLock {
    /// Takes the writer lock for `home` and writes this process's record.
    ///
    /// # Errors
    ///
    /// `home.locked` (exit 75) naming the holder when another process holds
    /// the lock; `home.io` when `run/` or its files cannot be written.
    ///
    /// # Panics
    ///
    /// Never: a record holds only strings and numbers.
    pub fn acquire(home: &Path, role: Role, profiles: Vec<String>) -> Result<Self, Error> {
        let dir = run_dir(home);
        private_fs::create_dir(&dir).map_err(|e| io_error(&dir, e))?;
        let path = dir.join(LOCK_FILE);
        let file = lock_file_options()
            .open(&path)
            .map_err(|e| io_error(&path, e))?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(held_error(home)),
            Err(TryLockError::Error(e)) => return Err(io_error(&path, e)),
        }
        // Holding the lock proves no other writer runs: every record left
        // in `run/` is from a process that died without cleaning up.
        for stale in Role::ALL {
            remove_record(home, stale)?;
        }
        let record = RuntimeRecord::current(role, profiles);
        let record_path = dir.join(role.record_file());
        let json = serde_json::to_vec_pretty(&record).expect("a record always serializes");
        private_fs::write_atomic(&record_path, &json).map_err(|e| io_error(&record_path, e))?;
        Ok(Self {
            _file: file,
            home: home.to_owned(),
            record,
        })
    }

    /// This process's record.
    #[must_use]
    pub fn record(&self) -> &RuntimeRecord {
        &self.record
    }
}

impl Drop for WriterLock {
    fn drop(&mut self) {
        // The lock file stays: deleting it while held would let a racing
        // process lock a fresh inode. Only the record goes.
        let _ = remove_record(&self.home, self.record.role);
    }
}

fn lock_file_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options
}

/// The conflict error for a held lock, naming the live holder if known.
fn held_error(home: &Path) -> Error {
    let holder = Role::ALL
        .into_iter()
        .find_map(|role| read_record(home, role).ok().flatten())
        .filter(|record| record.liveness() == Liveness::Alive);
    match holder {
        Some(record) if record.role == Role::Daemon => Error::new(
            &codes::HOME_LOCKED,
            format!(
                "gateway already running (pid {}); attach with 'prometheus tui' or stop it with 'prometheus daemon stop'",
                record.pid
            ),
        ),
        Some(record) => Error::new(
            &codes::HOME_LOCKED,
            format!(
                "home {} is in use by 'prometheus chat' (pid {})",
                home.display(),
                record.pid
            ),
        )
        .with_hint("close the other chat, or use another home with --home"),
        None => Error::new(
            &codes::HOME_LOCKED,
            format!(
                "home {} is locked by another prometheus process",
                home.display()
            ),
        )
        .with_hint("retry in a moment"),
    }
}

fn io_error(path: &Path, e: io::Error) -> Error {
    Error::new(
        &codes::HOME_IO,
        format!("cannot access {}: {e}", path.display()),
    )
    .with_source(e)
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::{Liveness, Role, RuntimeRecord, WriterLock, live_record, read_record, run_dir};
    use crate::private_fs;

    fn write_record(home: &std::path::Path, record: &RuntimeRecord) {
        let dir = run_dir(home);
        private_fs::create_dir(&dir).unwrap();
        let json = serde_json::to_vec(record).unwrap();
        private_fs::write_atomic(&dir.join(record.role.record_file()), &json).unwrap();
    }

    /// The pid of a child that has exited and been reaped.
    fn dead_pid() -> u32 {
        let exe = std::env::current_exe().unwrap();
        let mut child = Command::new(exe).arg("--list").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }

    #[test]
    fn second_writer_gets_exit_75_naming_the_holder() {
        let home = tempfile::tempdir().unwrap();
        let first = WriterLock::acquire(home.path(), Role::Daemon, vec!["default".into()]).unwrap();
        let err = WriterLock::acquire(home.path(), Role::Chat, vec!["default".into()]).unwrap_err();
        assert_eq!(err.exit_code(), 75);
        assert_eq!(err.code().code, "home.locked");
        assert_eq!(
            err.message(),
            format!(
                "gateway already running (pid {}); attach with 'prometheus tui' or stop it with 'prometheus daemon stop'",
                std::process::id()
            )
        );
        drop(first);
        WriterLock::acquire(home.path(), Role::Chat, vec![]).expect("free after release");
    }

    #[test]
    fn chat_holder_is_named_in_the_conflict() {
        let home = tempfile::tempdir().unwrap();
        let _chat = WriterLock::acquire(home.path(), Role::Chat, vec![]).unwrap();
        let err = WriterLock::acquire(home.path(), Role::Daemon, vec![]).unwrap_err();
        assert_eq!(err.exit_code(), 75);
        assert!(
            err.message().contains("in use by 'prometheus chat'"),
            "{err}"
        );
    }

    #[test]
    fn record_lives_while_the_lock_is_held() {
        let home = tempfile::tempdir().unwrap();
        let lock = WriterLock::acquire(home.path(), Role::Daemon, vec!["work".into()]).unwrap();
        let record = live_record(home.path(), Role::Daemon).unwrap().unwrap();
        assert_eq!(&record, lock.record());
        assert_eq!(record.pid, std::process::id());
        assert_eq!(record.profiles, ["work"]);
        assert_eq!(record.liveness(), Liveness::Alive);
        drop(lock);
        assert_eq!(read_record(home.path(), Role::Daemon).unwrap(), None);
    }

    #[test]
    fn stale_record_with_dead_pid_is_detected_and_removed() {
        let home = tempfile::tempdir().unwrap();
        let mut record = RuntimeRecord::current(Role::Daemon, vec![]);
        record.pid = dead_pid();
        write_record(home.path(), &record);
        assert_eq!(record.liveness(), Liveness::Dead);
        assert_eq!(live_record(home.path(), Role::Daemon).unwrap(), None);
        assert!(!run_dir(home.path()).join("gateway.json").exists());
    }

    #[test]
    fn reused_pid_with_another_start_time_is_stale() {
        let home = tempfile::tempdir().unwrap();
        let mut record = RuntimeRecord::current(Role::Daemon, vec![]);
        assert!(
            record.start_time > 0,
            "the OS reports this process's start time"
        );
        record.start_time -= 3600;
        write_record(home.path(), &record);
        assert_eq!(record.liveness(), Liveness::Reused);
        assert_eq!(live_record(home.path(), Role::Daemon).unwrap(), None);
    }

    #[test]
    fn acquiring_replaces_records_left_by_a_crash() {
        let home = tempfile::tempdir().unwrap();
        let mut crashed = RuntimeRecord::current(Role::Chat, vec![]);
        crashed.pid = dead_pid();
        write_record(home.path(), &crashed);
        let _lock = WriterLock::acquire(home.path(), Role::Daemon, vec![]).unwrap();
        assert_eq!(read_record(home.path(), Role::Chat).unwrap(), None);
    }

    #[cfg(unix)]
    #[test]
    fn run_files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let _lock = WriterLock::acquire(home.path(), Role::Chat, vec![]).unwrap();
        let mode = |name: &str| {
            let path = run_dir(home.path()).join(name);
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777
        };
        assert_eq!(mode(""), 0o700);
        assert_eq!(mode("gateway.lock"), 0o600);
        assert_eq!(mode("chat.json"), 0o600);
    }
}

//! Records of launched browsers (spec: execution-environments, Browser
//! supervision, dialogs and cleanup).
//!
//! Every local browser is recorded under `<profile>/cache/browser/records/`
//! (0700) with its owner process, so a later process can reap the browser
//! of an owner that crashed. A browser is killed only after its pid, start
//! time and `--user-data-dir` argument all match the record (a recycled pid
//! is never touched); its throwaway profile directory goes with it.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::cleanup;
use serde::{Deserialize, Serialize};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

/// One launched browser.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// The process that launched it.
    pub owner_pid: u32,
    /// The owner's start time (seconds since the epoch).
    pub owner_start: u64,
    /// The browser's pid.
    pub browser_pid: u32,
    /// The browser's start time.
    pub browser_start: u64,
    /// Its throwaway profile directory.
    pub profile_dir: PathBuf,
    /// Its temporary directory.
    #[serde(default)]
    pub temp_dir: Option<PathBuf>,
}

/// The records directory of a profile's browser cache.
#[derive(Debug, Clone)]
pub struct Records {
    dir: PathBuf,
}

impl Records {
    /// Records under `<browser_cache>/records`.
    #[must_use]
    pub fn new(browser_cache: &Path) -> Self {
        Self {
            dir: browser_cache.join("records"),
        }
    }

    /// Records a browser this process launched; returns the record's path.
    ///
    /// # Errors
    ///
    /// The record could not be written.
    pub fn add(
        &self,
        browser_pid: u32,
        profile_dir: &Path,
        temp_dir: &Path,
    ) -> io::Result<PathBuf> {
        pmt_core::private_fs::create_dir(&self.dir)?;
        let owner_pid = std::process::id();
        let record = Record {
            owner_pid,
            owner_start: start_time(owner_pid).unwrap_or_default(),
            browser_pid,
            browser_start: start_time(browser_pid).unwrap_or_default(),
            profile_dir: profile_dir.to_owned(),
            temp_dir: Some(temp_dir.to_owned()),
        };
        let path = self
            .dir
            .join(format!("{browser_pid}-{}.json", record.browser_start));
        let text = serde_json::to_vec(&record).map_err(io::Error::other)?;
        pmt_core::private_fs::write_atomic(&path, &text)?;
        Ok(path)
    }

    /// Kills the browsers of owners that are gone and removes their records
    /// and profile directories; returns how many records were cleared.
    #[must_use]
    pub fn reap_orphans(&self) -> usize {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return 0;
        };
        let mut cleared = 0;
        for path in entries.flatten().map(|e| e.path()) {
            let Some(record) = read(&path) else {
                let _ = fs::remove_file(&path);
                continue;
            };
            if same_process(record.owner_pid, record.owner_start) {
                continue;
            }
            if kill_if_same(&record) {
                wait_for_exit(record.browser_pid, record.browser_start);
            }
            cleanup::remove_dir(&record.profile_dir);
            if let Some(temp) = &record.temp_dir {
                cleanup::remove_dir(temp);
            }
            let _ = fs::remove_file(&path);
            cleared += 1;
        }
        cleared
    }
}

fn read(path: &Path) -> Option<Record> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

/// The start time of `pid`, if it runs.
fn start_time(pid: u32) -> Option<u64> {
    pmt_core::runtime::process_start_time(pid)
}

/// Whether `pid` still runs and started at `start`.
#[must_use]
pub fn same_process(pid: u32, start: u64) -> bool {
    start_time(pid) == Some(start)
}

/// How long a killed browser may take to exit before its profile
/// directory is removed anyway.
const EXIT_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// Waits until `pid` (started at `start`) is gone, at most [`EXIT_WAIT`]:
/// a dying browser still writes to its profile directory.
fn wait_for_exit(pid: u32, start: u64) {
    let deadline = std::time::Instant::now() + EXIT_WAIT;
    while same_process(pid, start) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Kills the browser of `record` with its helpers (its process group) when
/// pid, start time and profile flag all match; returns whether it did.
#[must_use]
pub fn kill_if_same(record: &Record) -> bool {
    let pid = Pid::from_u32(record.browser_pid);
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always),
    );
    let Some(process) = system.process(pid) else {
        return false;
    };
    let mut flag = std::ffi::OsString::from("--user-data-dir=");
    flag.push(&record.profile_dir);
    let owns_profile = process
        .cmd()
        .iter()
        .any(|arg| arg.as_os_str() == flag.as_os_str());
    if process.start_time() != record.browser_start || !owns_profile {
        return false;
    }
    cleanup::kill_group(record.browser_pid);
    #[cfg(not(unix))]
    process.kill();
    true
}

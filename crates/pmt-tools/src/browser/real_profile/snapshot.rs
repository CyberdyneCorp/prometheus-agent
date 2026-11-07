//! The managed copy of a real browser profile (spec: execution-environments,
//! Real-profile browsing).
//!
//! The snapshot is a user data directory of its own under
//! `<profile>/cache/browser/real-profile/<browser>/`: the browser's
//! `Local State` and the one profile directory browsed (the last used
//! one, or `browser.real_profile_pin`). Databases are copied with SQLite's
//! online backup, so a page of a database the real browser is writing is
//! never half-copied; a database the browser holds locked makes the copy
//! impossible. Caches and the browser's own lock files are skipped.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::detect::Browser;

/// A ready snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// The user data directory to launch on.
    pub user_data_dir: PathBuf,
    /// The profile directory inside it (`--profile-directory`).
    pub profile: String,
}

/// Directories never copied: caches the browser rebuilds.
const SKIPPED_DIRS: &[&str] = &[
    "Cache",
    "Code Cache",
    "GPUCache",
    "DawnCache",
    "DawnGraphiteCache",
    "DawnWebGPUCache",
    "GrShaderCache",
    "ShaderCache",
    "Service Worker",
    "blob_storage",
    "Crashpad",
];

/// Files never copied: the live browser's locks.
const SKIPPED_FILES: &[&str] = &[
    "SingletonLock",
    "SingletonSocket",
    "SingletonCookie",
    "LOCK",
    "lockfile",
];

/// How long a busy database may stay locked before the copy gives up.
const BUSY_WAIT: Duration = Duration::from_secs(2);

/// The snapshot root of `browser` under `cache_dir`.
#[must_use]
pub fn root(cache_dir: &Path) -> PathBuf {
    cache_dir.join("real-profile")
}

/// The profile directory to copy: `pin`, else `Local State`'s last used
/// profile, else `Default`.
///
/// # Errors
///
/// The pinned or last used profile does not exist.
pub fn chosen_profile(browser: &Browser, pin: &str) -> Result<String, String> {
    let pin = pin.trim();
    if !pin.is_empty() {
        if browser.user_data_dir.join(pin).is_dir() {
            return Ok(pin.to_owned());
        }
        return Err(format!(
            "browser.real_profile_pin '{pin}' does not exist in {}",
            browser.user_data_dir.display()
        ));
    }
    let last_used = fs::read(browser.user_data_dir.join("Local State"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|state| {
            state
                .pointer("/profile/last_used")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "Default".to_owned());
    if browser.user_data_dir.join(&last_used).is_dir() {
        Ok(last_used)
    } else {
        Err(format!(
            "the {} profile '{last_used}' does not exist in {}",
            browser.name,
            browser.user_data_dir.display()
        ))
    }
}

/// Copies (or re-syncs) `browser`'s chosen profile into its snapshot under
/// `cache_dir`.
///
/// # Errors
///
/// Why the copy is impossible: a missing pin or profile, a locked
/// database, a snapshot another browser session is using, or I/O.
pub fn sync(browser: &Browser, pin: &str, cache_dir: &Path) -> Result<Snapshot, String> {
    let profile = chosen_profile(browser, pin)?;
    let target = root(cache_dir).join(&browser.name);
    if in_use(&target) {
        return Err("the real-profile snapshot is in use by another browser session".to_owned());
    }
    let staging = root(cache_dir).join(format!(".{}-staging", browser.name));
    let _ = fs::remove_dir_all(&staging);
    pmt_core::private_fs::create_dir(&staging).map_err(io_error)?;
    let copied = copy_state(browser, &staging).and_then(|()| {
        copy_tree(
            &browser.user_data_dir.join(&profile),
            &staging.join(&profile),
        )
    });
    if let Err(why) = copied {
        let _ = fs::remove_dir_all(&staging);
        return Err(why);
    }
    let _ = fs::remove_dir_all(&target);
    fs::rename(&staging, &target).map_err(io_error)?;
    Ok(Snapshot {
        user_data_dir: target,
        profile,
    })
}

/// Deletes every snapshot (the real-profile setting was turned off).
pub fn forget(cache_dir: &Path) {
    let dir = root(cache_dir);
    if dir.exists() {
        super::super::cleanup::remove_dir(&dir);
    }
}

/// Whether a running browser holds `dir` (its singleton lock exists and
/// names a live process on this host).
fn in_use(dir: &Path) -> bool {
    let lock = dir.join("SingletonLock");
    #[cfg(unix)]
    {
        let Ok(target) = fs::read_link(&lock) else {
            return false;
        };
        let pid = target
            .to_string_lossy()
            .rsplit('-')
            .next()
            .and_then(|pid| pid.parse::<u32>().ok());
        pid.is_some_and(|pid| pmt_core::runtime::process_start_time(pid).is_some())
    }
    #[cfg(not(unix))]
    {
        lock.exists() && fs::remove_file(&lock).is_err()
    }
}

fn copy_state(browser: &Browser, staging: &Path) -> Result<(), String> {
    let state = browser.user_data_dir.join("Local State");
    if state.is_file() {
        pmt_core::private_fs::copy_file(&state, &staging.join("Local State")).map_err(io_error)?;
    }
    Ok(())
}

fn copy_tree(from: &Path, to: &Path) -> Result<(), String> {
    pmt_core::private_fs::create_dir(to).map_err(io_error)?;
    for entry in fs::read_dir(from).map_err(io_error)?.flatten() {
        let name = entry.file_name();
        let name_text = name.to_string_lossy();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let (source, dest) = (entry.path(), to.join(&name));
        if kind.is_dir() && !SKIPPED_DIRS.contains(&name_text.as_ref()) {
            copy_tree(&source, &dest)?;
        } else if kind.is_file() && !SKIPPED_FILES.contains(&name_text.as_ref()) {
            copy_file(&source, &dest)?;
        }
    }
    Ok(())
}

/// Copies one file: a SQLite database through the online backup API,
/// anything else as bytes.
fn copy_file(source: &Path, dest: &Path) -> Result<(), String> {
    if !is_sqlite(source) {
        return pmt_core::private_fs::copy_file(source, dest).map_err(io_error);
    }
    backup(source, dest).map_err(|err| {
        let name = source.file_name().unwrap_or_default().to_string_lossy();
        if matches!(
            err.sqlite_error_code(),
            Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
        ) {
            format!("the browser holds the database '{name}' locked (it is running)")
        } else {
            format!("cannot copy the database '{name}': {err}")
        }
    })
}

fn backup(source: &Path, dest: &Path) -> rusqlite::Result<()> {
    use rusqlite::OpenFlags;
    let from = rusqlite::Connection::open_with_flags(
        source,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    from.busy_timeout(BUSY_WAIT)?;
    let mut to = rusqlite::Connection::open(dest)?;
    let copy = rusqlite::backup::Backup::new(&from, &mut to)?;
    // Stepped by hand: `run_to_completion` retries a busy source forever.
    let deadline = std::time::Instant::now() + BUSY_WAIT;
    loop {
        match copy.step(256)? {
            rusqlite::backup::StepResult::Done => return Ok(()),
            rusqlite::backup::StepResult::More => {}
            _ if std::time::Instant::now() >= deadline => {
                return Err(rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
                    None,
                ));
            }
            _ => std::thread::sleep(Duration::from_millis(10)),
        }
    }
}

fn is_sqlite(path: &Path) -> bool {
    use std::io::Read as _;
    let mut header = [0u8; 16];
    fs::File::open(path)
        .and_then(|mut file| file.read_exact(&mut header))
        .is_ok_and(|()| &header == b"SQLite format 3\0")
}

#[allow(clippy::needless_pass_by_value, reason = "used as `map_err(io_error)`")]
fn io_error(err: io::Error) -> String {
    format!("cannot copy the real profile: {err}")
}

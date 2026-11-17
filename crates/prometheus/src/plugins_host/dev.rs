//! Plugins served by `prometheus plugins dev` (spec: plugin-system,
//! Plugin development loop): the command registers a local directory in
//! `<profile>/cache/plugin-dev/<id>.toml` while it runs; processes a
//! developer works in (`prometheus chat` and the dev command itself:
//! [`allow`]; TUI sessions run in the daemon) load it as `dev:<id>` beside the installed plugins,
//! and pick up each rebuild at the next hook they fire.
//! Gateway, cron and every other unattended process never call [`allow`],
//! so dev plugins never load there.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use pmt_core::error::{Error, codes};
use serde::{Deserialize, Serialize};

static ALLOWED: AtomicBool = AtomicBool::new(false);

/// Lets this process load dev plugins.
pub fn allow() {
    ALLOWED.store(true, Ordering::Relaxed);
}

/// Whether this process loads dev plugins.
#[must_use]
pub fn allowed() -> bool {
    ALLOWED.load(Ordering::Relaxed)
}

/// One served directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registration {
    /// The plugin directory.
    pub path: PathBuf,
    /// The serving process.
    pub pid: u32,
    /// Its start time (with the pid, it tells a reused pid apart).
    pub started: u64,
    /// Bumped by every rebuild.
    pub generation: u64,
}

/// `<profile>/cache/plugin-dev`.
#[must_use]
pub fn dir(profile_dir: &Path) -> PathBuf {
    profile_dir.join("cache").join("plugin-dev")
}

fn file(profile_dir: &Path, id: &str) -> PathBuf {
    dir(profile_dir).join(format!("{id}.toml"))
}

impl Registration {
    /// A registration of `path` by this process.
    #[must_use]
    pub fn of(path: &Path) -> Self {
        let pid = std::process::id();
        Self {
            path: path.to_path_buf(),
            pid,
            started: pmt_core::runtime::process_start_time(pid).unwrap_or(0),
            generation: 0,
        }
    }

    fn alive(&self) -> bool {
        pmt_core::runtime::process_start_time(self.pid) == Some(self.started)
    }

    /// Writes it as plugin `id`'s.
    ///
    /// # Errors
    ///
    /// `home.io`.
    pub fn save(&self, profile_dir: &Path, id: &str) -> Result<(), Error> {
        let path = file(profile_dir, id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::io_doing(&codes::HOME_IO, "cannot create", parent, &e))?;
        }
        let text = toml::to_string(self).unwrap_or_default();
        pmt_core::private_fs::write_atomic(&path, text.as_bytes())
            .map_err(|e| Error::io(&codes::HOME_IO, &path, &e))
    }
}

/// Removes plugin `id`'s registration.
pub fn unregister(profile_dir: &Path, id: &str) {
    let _ = std::fs::remove_file(file(profile_dir, id));
}

/// The directories served now, when this process loads dev plugins.
#[must_use]
pub fn live(profile_dir: &Path) -> Vec<PathBuf> {
    if !allowed() {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(dir(profile_dir)) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .filter_map(|text| toml::from_str::<Registration>(&text).ok())
        .filter(Registration::alive)
        .map(|r| r.path)
        .collect();
    dirs.sort();
    dirs
}

/// What the registrations look like now (names, sizes and modification
/// times): a rebuild or a registration coming or going changes it.
#[must_use]
pub fn stamp(profile_dir: &Path) -> String {
    if !allowed() {
        return String::new();
    }
    let Ok(entries) = std::fs::read_dir(dir(profile_dir)) else {
        return String::new();
    };
    let mut parts: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            Some(format!(
                "{}:{}:{:?}",
                e.file_name().to_string_lossy(),
                meta.len(),
                meta.modified().ok()
            ))
        })
        .collect();
    parts.sort();
    parts.join(";")
}

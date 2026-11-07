//! The notepad channel of a run (spec: scheduled-jobs, Per-job notepad,
//! "also callable from the run's terminal tool"). The run's terminal is
//! sandboxed away from the home and its `state.db`, so the host hands the
//! run a directory of its own: `notepad.json` (the job's notepad when the
//! run started, which `prometheus cron notepad` edits there through
//! `PMT_CRON_NOTEPAD`) and `bin/` (the running `prometheus`, first on the
//! terminal's `PATH`). When the run ends the host applies what changed to
//! the job's notepad, under the same limits, and removes the directory.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_cron::job::Job;
use pmt_cron::notepad;
use pmt_store::Store;
use serde::{Deserialize, Serialize};

/// The channel file's name in the run's directory.
const FILE: &str = "notepad.json";
/// The directory put first on the run terminal's `PATH`.
const BIN: &str = "bin";

/// The channel file: which job the run belongs to and its notepad.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Page {
    /// The job's id.
    pub job_id: String,
    /// The job's name.
    pub job_name: String,
    /// The notepad, by key.
    pub notes: BTreeMap<String, String>,
}

fn io(path: &Path, err: &impl std::fmt::Display) -> Error {
    Error::new(
        &codes::OUTPUT_IO,
        format!("notepad channel {}: {err}", path.display()),
    )
}

impl Page {
    /// Reads the channel at `file`.
    ///
    /// # Errors
    ///
    /// `output.io` when it cannot be read or parsed.
    pub fn read(file: &Path) -> Result<Self, Error> {
        let text = std::fs::read_to_string(file).map_err(|e| io(file, &e))?;
        serde_json::from_str(&text).map_err(|e| io(file, &e))
    }

    /// Replaces the channel at `file` (a whole new file: a reader never
    /// sees half a write).
    ///
    /// # Errors
    ///
    /// `output.io` when it cannot be written.
    pub fn write(&self, file: &Path) -> Result<(), Error> {
        let text = serde_json::to_string(self).map_err(|e| io(file, &e))?;
        let partial = file.with_extension("json.partial");
        std::fs::write(&partial, text).map_err(|e| io(&partial, &e))?;
        std::fs::rename(&partial, file).map_err(|e| io(file, &e))
    }
}

/// The channel file in a run's directory `dir`.
#[must_use]
pub fn file_in(dir: &Path) -> PathBuf {
    dir.join(FILE)
}

/// The `PATH` directory in a run's directory `dir`.
#[must_use]
pub fn bin_in(dir: &Path) -> PathBuf {
    dir.join(BIN)
}

/// The running executable, resolved (the sandbox grants the real file).
#[must_use]
pub fn executable() -> Option<PathBuf> {
    std::env::current_exe().and_then(std::fs::canonicalize).ok()
}

/// One run's channel, open while the run's agent works.
pub(super) struct Channel {
    dir: PathBuf,
    job_id: String,
    before: BTreeMap<String, String>,
}

impl Channel {
    /// Opens the channel of run `run_id` of `job`:
    /// `<profile>/cache/cron/notepad/<run_id>/`.
    pub(super) fn open(
        scope: &ProfileScope,
        store: &Store,
        job: &Job,
        run_id: &str,
    ) -> Result<Self, Error> {
        let dir = scope.path("cache/cron/notepad").join(run_id);
        let bin = bin_in(&dir);
        std::fs::create_dir_all(&bin).map_err(|e| io(&dir, &e))?;
        let before: BTreeMap<String, String> = store.cron_notepad(job.id())?.into_iter().collect();
        let page = Page {
            job_id: job.id().to_owned(),
            job_name: job.row.name.clone(),
            notes: before.clone(),
        };
        page.write(&file_in(&dir))?;
        if let Some(exe) = executable() {
            link(&exe, &bin);
        }
        Ok(Self {
            dir,
            job_id: job.id().to_owned(),
            before,
        })
    }

    /// The run's directory.
    pub(super) fn dir(&self) -> &Path {
        &self.dir
    }

    /// Applies what the run changed to the job's notepad (each write under
    /// the notepad limits; a refused one is logged) and removes the
    /// directory.
    pub(super) fn settle(self, store: &Store) {
        match Page::read(&file_in(&self.dir)) {
            Ok(page) => self.apply(store, &page.notes),
            Err(err) => tracing::warn!(job = self.job_id, "notepad channel unread: {err}"),
        }
        if let Err(err) = std::fs::remove_dir_all(&self.dir) {
            tracing::debug!(dir = %self.dir.display(), "notepad channel not removed: {err}");
        }
    }

    fn apply(&self, store: &Store, after: &BTreeMap<String, String>) {
        let changed = after
            .iter()
            .filter(|(k, v)| self.before.get(*k) != Some(*v));
        for (key, value) in changed {
            if let Err(err) = notepad::set(store, &self.job_id, key, value) {
                tracing::warn!(
                    job = self.job_id,
                    key,
                    "notepad write of the run refused: {err}"
                );
            }
        }
        for key in self.before.keys().filter(|k| !after.contains_key(*k)) {
            if let Err(err) = notepad::delete(store, &self.job_id, key) {
                tracing::warn!(
                    job = self.job_id,
                    key,
                    "notepad delete of the run failed: {err}"
                );
            }
        }
    }
}

/// Puts `exe` in `bin` as `prometheus`, so the run's commands find it by
/// name; without the link they can still not reach the notepad, which the
/// run's commands then report.
fn link(exe: &Path, bin: &Path) {
    let name = exe.file_name().map_or_else(
        || std::ffi::OsString::from("prometheus"),
        std::ffi::OsStr::to_os_string,
    );
    let at = bin.join(name);
    #[cfg(unix)]
    let linked = std::os::unix::fs::symlink(exe, &at);
    #[cfg(not(unix))]
    let linked = std::fs::hard_link(exe, &at).or_else(|_| std::fs::copy(exe, &at).map(drop));
    if let Err(err) = linked {
        tracing::warn!(at = %at.display(), "prometheus not linked for the run: {err}");
    }
}

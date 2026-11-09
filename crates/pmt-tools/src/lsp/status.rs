//! What `prometheus lsp status` and RPC `lsp.status` show, and how
//! `prometheus lsp restart` reaches running processes (spec:
//! lsp-diagnostics, LSP CLI; Wait modes and timeouts).
//!
//! Every running server writes one entry under
//! `<profile>/cache/lsp/status/` (0700); it goes when the server stops, and
//! a later reader drops entries whose server no longer runs. `restart`
//! bumps a mark under `<profile>/cache/lsp/restart/` (one per id, `_all`
//! for every server); each process checks the marks before using a
//! server and then stops the servers and clears the broken marks named.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// One running server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// The server id.
    pub id: String,
    /// Its workspace root.
    pub workspace: String,
    /// The server's pid.
    pub pid: u32,
    /// The server's start time (seconds since the epoch), which also
    /// tells a reused pid apart.
    pub started: u64,
    /// `starting` or `ready`.
    pub state: String,
    /// Whether the server executes project code, so it starts only in
    /// trusted workspaces.
    pub trusted_only: bool,
    /// The session that started it.
    pub session_id: String,
}

/// The status and restart files of a profile.
#[derive(Debug, Clone)]
pub struct Status {
    dir: PathBuf,
}

/// The mark naming every server.
const ALL: &str = "_all";

impl Status {
    /// The files under `<profile>/cache/lsp`.
    #[must_use]
    pub fn of_profile(profile_dir: &Path) -> Self {
        Self {
            dir: profile_dir.join("cache").join("lsp"),
        }
    }

    /// The directory servers' caches live under.
    #[must_use]
    pub fn cache_root(&self) -> &Path {
        &self.dir
    }

    fn entry_path(&self, pid: u32) -> PathBuf {
        self.dir.join("status").join(format!("{pid}.json"))
    }

    /// Records a running server; status is advisory, failures are logged.
    pub fn write(&self, entry: &Entry) {
        let written = pmt_core::private_fs::create_dir(&self.dir.join("status")).and_then(|()| {
            let text = serde_json::to_vec(entry).map_err(std::io::Error::other)?;
            pmt_core::private_fs::write_atomic(&self.entry_path(entry.pid), &text)
        });
        if let Err(err) = written {
            tracing::warn!(error = %err, "could not record the language server's status");
        }
    }

    /// Removes the entry of the server `pid`.
    pub fn remove(&self, pid: u32) {
        let _ = std::fs::remove_file(self.entry_path(pid));
    }

    /// The entries of servers that still run, by id and workspace; stale
    /// ones are removed.
    #[must_use]
    pub fn live(&self) -> Vec<Entry> {
        let Ok(files) = std::fs::read_dir(self.dir.join("status")) else {
            return Vec::new();
        };
        let mut live: Vec<Entry> = files
            .flatten()
            .filter_map(|file| {
                let path = file.path();
                let entry = std::fs::read(&path)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<Entry>(&bytes).ok())
                    .filter(|e| crate::browser::records::same_process(e.pid, e.started));
                if entry.is_none() {
                    let _ = std::fs::remove_file(&path);
                }
                entry
            })
            .collect();
        live.sort_by(|a, b| (&a.id, &a.workspace, a.pid).cmp(&(&b.id, &b.workspace, b.pid)));
        live
    }

    fn mark_path(&self, id: Option<&str>) -> PathBuf {
        self.dir.join("restart").join(id.unwrap_or(ALL))
    }

    /// Asks every process of the profile to stop `id`'s servers (all
    /// servers for `None`) and clear their broken marks.
    ///
    /// # Errors
    ///
    /// The mark cannot be written.
    pub fn request_restart(&self, id: Option<&str>) -> std::io::Result<()> {
        pmt_core::private_fs::create_dir(&self.dir.join("restart"))?;
        let stamp = now_nanos();
        pmt_core::private_fs::write_atomic(&self.mark_path(id), stamp.to_string().as_bytes())
    }

    /// The restart mark of `id` (`None`: every server); 0 when never set.
    #[must_use]
    pub fn restart_mark(&self, id: Option<&str>) -> u128 {
        std::fs::read_to_string(self.mark_path(id))
            .ok()
            .and_then(|text| text.trim().parse().ok())
            .unwrap_or(0)
    }
}

/// Ends the server of `entry` when it is still that process (its start
/// time matches); returns whether it was signalled.
#[must_use]
pub fn stop(entry: &Entry) -> bool {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
    let pid = Pid::from_u32(entry.pid);
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing(),
    );
    system
        .process(pid)
        .filter(|process| process.start_time() == entry.started)
        .is_some_and(sysinfo::Process::kill)
}

/// Now, in nanoseconds since the epoch.
#[must_use]
pub fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
}

/// Now, in seconds since the epoch.
#[must_use]
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_of_running_servers_are_listed_and_stale_ones_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let status = Status::of_profile(dir.path());
        let me = std::process::id();
        let entry = Entry {
            id: "pyright".into(),
            workspace: "/w".into(),
            pid: me,
            started: pmt_core::runtime::process_start_time(me).unwrap_or_default(),
            state: "ready".into(),
            trusted_only: false,
            session_id: "s".into(),
        };
        status.write(&entry);
        status.write(&Entry {
            pid: u32::MAX - 1,
            ..entry.clone()
        });
        assert_eq!(status.live(), vec![entry]);
        status.remove(me);
        assert!(status.live().is_empty());
    }

    #[test]
    fn restart_marks_grow() {
        let dir = tempfile::tempdir().unwrap();
        let status = Status::of_profile(dir.path());
        assert_eq!(status.restart_mark(Some("gopls")), 0);
        status.request_restart(Some("gopls")).unwrap();
        let first = status.restart_mark(Some("gopls"));
        assert!(first > 0);
        assert_eq!(status.restart_mark(None), 0);
    }
}

//! Sync-back of a remote workspace (spec: execution-environments, Remote
//! file sync and sync-back).
//!
//! At session end and on `/sync`, files changed under the remote workspace
//! since the previous sync are copied to the host workspace, in path
//! order, until `terminal.sync_back_max_bytes` is spent; whatever does not
//! fit is skipped and listed. A host file modified after the previous sync
//! is never overwritten: it is reported as a conflict. Changes are found
//! with a marker file in the remote session state (`find -newer`), so the
//! workspace itself gains nothing.

use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::SystemTime;

use super::remote::RemoteExec;

/// Starts a sync window: the marker records "now" remotely, and the
/// workspace root is printed with `~` resolved. Arguments: marker, root.
const MARK: &str = "mkdir -p \"$(dirname \"$1\")\" && : > \"$1\" || exit 1\n\
    case \"$2\" in \"~\") printf '%s' \"$HOME\" ;; \"~/\"*) printf '%s/%s' \"$HOME\" \"${2#\"~/\"}\" ;; *) printf '%s' \"$2\" ;; esac";

/// Lists `size|path` of files newer than the marker, then advances it.
/// Arguments: workspace, marker.
const CHANGED: &str = "[ -d \"$1\" ] || exit 0\n\
    : > \"$2.next\"\n\
    find \"$1\" -type f -newer \"$2\" | while IFS= read -r f; do\n\
      printf '%s|%s\\n' \"$(wc -c < \"$f\" | tr -d ' ')\" \"$f\"\n\
    done\n\
    mv -f \"$2.next\" \"$2\"";

/// What one sync did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// Copied files, relative to the workspace.
    pub copied: Vec<String>,
    /// Files left behind because the budget was spent, with their sizes.
    pub skipped: Vec<(String, u64)>,
    /// Host files modified since the previous sync (left untouched).
    pub conflicts: Vec<String>,
    /// Files that could not be copied, with why.
    pub failed: Vec<(String, String)>,
}

impl Report {
    /// The notice for the session: what was skipped or left in conflict.
    #[must_use]
    pub fn notice(&self, budget: u64) -> Option<String> {
        let mut parts = Vec::new();
        if !self.skipped.is_empty() {
            let names: Vec<String> = self
                .skipped
                .iter()
                .map(|(path, size)| format!("{path} ({size} bytes)"))
                .collect();
            parts.push(format!(
                "sync-back stopped at the {budget}-byte budget (terminal.sync_back_max_bytes); skipped: {}",
                names.join(", ")
            ));
        }
        if !self.conflicts.is_empty() {
            parts.push(format!(
                "sync-back left host files changed since the last sync untouched (conflict): {}",
                self.conflicts.join(", ")
            ));
        }
        for (path, why) in &self.failed {
            parts.push(format!("sync-back could not copy {path}: {why}"));
        }
        (!parts.is_empty()).then(|| parts.join("; "))
    }
}

/// The sync-back of one session.
#[derive(Debug)]
pub struct SyncBack {
    exec: Arc<dyn RemoteExec>,
    remote_root: Mutex<String>,
    host_root: PathBuf,
    marker: String,
    budget: u64,
    last: Mutex<Option<SystemTime>>,
}

impl SyncBack {
    /// Syncs `remote_root` (absolute in the backend, or under `~`) into
    /// `host_root`; `marker` is a path in the remote session state (`~`
    /// is not expanded there).
    #[must_use]
    pub fn new(
        exec: Arc<dyn RemoteExec>,
        remote_root: &str,
        host_root: &Path,
        marker: &str,
        budget: u64,
    ) -> Self {
        Self {
            exec,
            remote_root: Mutex::new(remote_root.trim_end_matches('/').to_owned()),
            host_root: host_root.to_path_buf(),
            marker: marker.to_owned(),
            budget,
            last: Mutex::new(None),
        }
    }

    /// Opens the window: later changes are what the first sync copies.
    ///
    /// # Errors
    ///
    /// The backend could not be reached.
    pub fn start(&self) -> Result<(), String> {
        let root = self.root();
        let result = self.exec.exec(MARK, &[&self.marker, &root], None)?;
        if result.code != 0 {
            return Err(String::from_utf8_lossy(&result.stderr).trim().to_owned());
        }
        let resolved = String::from_utf8_lossy(&result.stdout);
        resolved.trim_end_matches('/').clone_into(
            &mut self
                .remote_root
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        *self.last.lock().unwrap_or_else(PoisonError::into_inner) = Some(SystemTime::now());
        Ok(())
    }

    fn root(&self) -> String {
        self.remote_root
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Copies what changed since the previous sync.
    ///
    /// # Errors
    ///
    /// The backend could not list the changes.
    pub fn sync(&self) -> Result<Report, String> {
        let since = *self.last.lock().unwrap_or_else(PoisonError::into_inner);
        if since.is_none() {
            // The window never opened (the backend was unreachable at
            // session start): open it now; nothing is known to be changed.
            self.start()?;
            return Ok(Report::default());
        }
        let root = self.root();
        let listed = self.exec.exec(CHANGED, &[&root, &self.marker], None)?;
        if listed.code != 0 {
            return Err(String::from_utf8_lossy(&listed.stderr).trim().to_owned());
        }
        let mut changed: Vec<(String, u64)> = String::from_utf8_lossy(&listed.stdout)
            .lines()
            .filter_map(|line| {
                let (size, path) = line.split_once('|')?;
                Some((path.to_owned(), size.trim().parse().ok()?))
            })
            .collect();
        changed.sort();
        let mut report = Report::default();
        let mut spent = 0u64;
        for (path, size) in changed {
            let Some(relative) = relative(&root, &path) else {
                continue;
            };
            let host = self.host_root.join(&relative);
            if modified_since(&host, since) {
                report.conflicts.push(relative);
                continue;
            }
            if spent.saturating_add(size) > self.budget {
                report.skipped.push((relative, size));
                continue;
            }
            match self.copy(&path, &host) {
                Ok(()) => {
                    spent += size;
                    report.copied.push(relative);
                }
                Err(why) => report.failed.push((relative, why)),
            }
        }
        // After the copies: the files this sync wrote are not host edits.
        *self.last.lock().unwrap_or_else(PoisonError::into_inner) = Some(SystemTime::now());
        Ok(report)
    }

    fn copy(&self, remote: &str, host: &Path) -> Result<(), String> {
        let bytes = if let Some(read) = self.exec.download(remote) {
            read?
        } else {
            let read = self.exec.exec("exec cat -- \"$1\"", &[remote], None)?;
            if read.code != 0 {
                return Err(String::from_utf8_lossy(&read.stderr).trim().to_owned());
            }
            read.stdout
        };
        if let Some(parent) = host.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let tmp = host.with_extension("pmt-sync");
        std::fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, host).map_err(|e| e.to_string())
    }
}

/// `path` relative to the remote `root`, refusing anything that would land
/// outside the host workspace.
fn relative(root: &str, path: &str) -> Option<String> {
    let rest = path.strip_prefix(root)?.trim_start_matches('/');
    let safe = Path::new(rest)
        .components()
        .all(|c| matches!(c, Component::Normal(_)));
    (safe && !rest.is_empty()).then(|| rest.to_owned())
}

/// Whether the host file changed after `since` (a file that exists while
/// no sync happened yet is not a conflict: the window starts at session
/// start).
fn modified_since(host: &Path, since: Option<SystemTime>) -> bool {
    let (Some(since), Ok(meta)) = (since, std::fs::metadata(host)) else {
        return false;
    };
    meta.modified().is_ok_and(|m| m > since)
}

#[cfg(test)]
mod tests {
    use super::Report;

    #[test]
    fn the_notice_lists_every_skipped_file() {
        let report = Report {
            skipped: vec![("build/big.bin".to_owned(), 3_000)],
            conflicts: vec!["README.md".to_owned()],
            ..Report::default()
        };
        let notice = report.notice(2_000).unwrap();
        assert!(
            notice.contains("stopped at the 2000-byte budget"),
            "{notice}"
        );
        assert!(notice.contains("build/big.bin (3000 bytes)"), "{notice}");
        assert!(notice.contains("README.md"), "{notice}");
        assert!(Report::default().notice(1).is_none());
    }
}

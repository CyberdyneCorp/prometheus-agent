//! What `prometheus browser status` shows (spec: execution-environments,
//! First-party cloud browser providers; Connecting to the user's browser).
//!
//! Every live browser session of a profile writes one entry under
//! `<profile>/cache/browser/sessions/` (0700) naming its origin and, for a
//! cloud session, its provider; the entry goes when the session ends. A
//! later process lists the entries whose owner still runs. Endpoints of
//! cloud sessions are never written: a provider's DevTools URL can carry a
//! signing key.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One live browser session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// Its key (`<session id>/<name>`).
    pub key: String,
    /// `local`, `cloud` or `connected`.
    pub origin: String,
    /// The cloud provider (`browser.provider`), for a cloud session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The explicit endpoint, for a connected session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// The local browser's pid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// `throwaway`, `user_data_dir` or `real:<browser>`, for a local one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// It has a window.
    #[serde(default)]
    pub headed: bool,
    /// A failed cloud provider was replaced by this local session.
    #[serde(default)]
    pub fallback_from_cloud: bool,
    /// The process that owns it, and when that process started.
    pub owner_pid: u32,
    /// The owner's start time (seconds since the epoch).
    pub owner_start: u64,
}

/// The status entries of a profile's browser cache.
#[derive(Debug, Clone)]
pub struct Sessions {
    dir: PathBuf,
}

impl Sessions {
    /// Entries under `<browser_cache>/sessions`.
    #[must_use]
    pub fn new(browser_cache: &Path) -> Self {
        Self {
            dir: browser_cache.join("sessions"),
        }
    }

    fn path(&self, key: &str) -> PathBuf {
        let safe: String = key
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '.'
                }
            })
            .collect();
        self.dir.join(format!("{safe}.json"))
    }

    /// Writes the entry of a session this process owns (`owner_*` filled
    /// in); failures are logged, status is advisory.
    pub fn write(&self, mut entry: Entry) {
        entry.owner_pid = std::process::id();
        entry.owner_start =
            pmt_core::runtime::process_start_time(entry.owner_pid).unwrap_or_default();
        let written = pmt_core::private_fs::create_dir(&self.dir).and_then(|()| {
            let text = serde_json::to_vec(&entry).map_err(std::io::Error::other)?;
            pmt_core::private_fs::write_atomic(&self.path(&entry.key), &text)
        });
        if let Err(err) = written {
            tracing::warn!(error = %err, "could not record the browser session's status");
        }
    }

    /// Removes the entry of `key`.
    pub fn remove(&self, key: &str) {
        let _ = std::fs::remove_file(self.path(key));
    }

    /// The entries whose owner still runs, by key; stale ones are removed.
    #[must_use]
    pub fn live(&self) -> Vec<Entry> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut live: Vec<Entry> = entries
            .flatten()
            .filter_map(|e| {
                let path = e.path();
                let entry: Option<Entry> = std::fs::read(&path)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice(&bytes).ok());
                match entry {
                    Some(entry)
                        if super::records::same_process(entry.owner_pid, entry.owner_start) =>
                    {
                        Some(entry)
                    }
                    _ => {
                        let _ = std::fs::remove_file(&path);
                        None
                    }
                }
            })
            .collect();
        live.sort_by(|a, b| a.key.cmp(&b.key));
        live
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_of_running_owners_are_listed_and_removed_at_the_end() {
        let root = tempfile::tempdir().unwrap();
        let sessions = Sessions::new(root.path());
        sessions.write(Entry {
            key: "ses_a/default".to_owned(),
            origin: "cloud".to_owned(),
            provider: Some("browserbase".to_owned()),
            endpoint: None,
            pid: None,
            profile: None,
            headed: false,
            fallback_from_cloud: false,
            owner_pid: 0,
            owner_start: 0,
        });
        let live = sessions.live();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].provider.as_deref(), Some("browserbase"));
        sessions.remove("ses_a/default");
        assert!(sessions.live().is_empty());
    }
}

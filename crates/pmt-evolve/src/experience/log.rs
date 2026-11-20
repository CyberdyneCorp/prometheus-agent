//! Library events (spec: experience-library, Library events). Entry
//! events happen outside any session (an adoption from the CLI, a
//! `forget`, a rollback), so they go to `<profile>/experience/events.jsonl`
//! in the trajectory grammar's line shape (`ts`, `type`, `payload`), and to
//! the INFO log. `experience.lesson.distilled` goes to its job's log and
//! `experience.session_lessons.injected` to its session's trajectory.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// `<profile>/experience/events.jsonl`.
#[derive(Debug, Clone)]
pub struct LibraryLog {
    path: PathBuf,
}

impl LibraryLog {
    /// The log of the library at `root`.
    #[must_use]
    pub fn new(root: &Path) -> Self {
        Self {
            path: root.join("events.jsonl"),
        }
    }

    /// Appends one event; a failure is logged, never fatal.
    pub fn emit(&self, kind: &str, payload: &Value) {
        let line = json!({
            "ts": jiff::Timestamp::now().to_string(),
            "type": kind,
            "payload": payload,
        });
        tracing::info!(event = kind, "{}", line["payload"]);
        let appended = self
            .path
            .parent()
            .map_or(Ok(()), pmt_core::private_fs::create_dir)
            .and_then(|()| pmt_core::private_fs::append_options().open(&self.path))
            .and_then(|mut file| {
                use std::io::Write as _;
                writeln!(file, "{line}")
            });
        if let Err(err) = appended {
            tracing::warn!(path = %self.path.display(), error = %err, "experience event not recorded");
        }
    }

    /// Every event recorded so far.
    #[must_use]
    pub fn read(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.path)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }
}

//! Live transcripts of dispatched children (spec: subagent-delegation,
//! Agents monitor and live transcripts).
//!
//! Every dispatch writes `<profile>/cache/delegation/live/<delegation_id>/`
//! with a `manifest.json` (the call and its tasks) and one append-only,
//! human-readable `task-<n>.log` per task (`n` counts from 1). A log exists
//! before the child's first model call and gains one timestamped line per
//! tool call, plus lines for its start, model calls and end, so a user can
//! `tail -f` a running child. Directories older than seven days are pruned
//! when a later dispatch starts.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use serde_json::json;

/// How long a delegation's transcripts are kept.
pub const KEEP: Duration = Duration::from_hours(7 * 24);

/// Where a profile's live transcripts are.
#[must_use]
pub fn root(profile_dir: &Path) -> PathBuf {
    profile_dir.join("cache/delegation/live")
}

/// One task of a dispatch, as its manifest lists it.
#[derive(Debug, Clone)]
pub struct TaskEntry {
    /// Its index in the call (from 0).
    pub task_index: usize,
    /// Its child.
    pub subagent_id: String,
    /// Its goal.
    pub goal: String,
}

/// One child's append-only log.
#[derive(Debug)]
pub struct Transcript {
    path: PathBuf,
    file: Mutex<Option<std::fs::File>>,
}

impl Transcript {
    /// The log's path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends one timestamped line (a failed write is logged, never
    /// fatal: the transcript is a convenience).
    pub fn line(&self, text: &str) {
        let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let mut file = self.file.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(f) = file.as_mut()
            && let Err(err) = writeln!(f, "{} {flat}", jiff::Timestamp::now())
        {
            tracing::debug!(error = %err, path = %self.path.display(), "transcript line lost");
        }
    }
}

/// Creates the transcripts of dispatch `delegation_id` under
/// `profile_dir` (after pruning expired ones); one per task, in task
/// order.
///
/// # Errors
///
/// The directory or a file could not be created.
pub fn open(
    profile_dir: &Path,
    delegation_id: &str,
    parent_session: &str,
    tasks: &[TaskEntry],
) -> std::io::Result<Vec<Transcript>> {
    let root = root(profile_dir);
    prune(&root, SystemTime::now());
    let dir = root.join(delegation_id);
    std::fs::create_dir_all(&dir)?;
    let mut transcripts = Vec::with_capacity(tasks.len());
    let mut listed = Vec::with_capacity(tasks.len());
    for task in tasks {
        let path = dir.join(format!("task-{}.log", task.task_index + 1));
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        let transcript = Transcript {
            path,
            file: Mutex::new(Some(file)),
        };
        transcript.line(&format!(
            "dispatched {} (task {}): {}",
            task.subagent_id,
            task.task_index + 1,
            task.goal
        ));
        listed.push(json!({
            "task_index": task.task_index,
            "subagent_id": task.subagent_id,
            "goal": task.goal,
            "log": transcript.path.file_name().map(|n| n.to_string_lossy().into_owned()),
        }));
        transcripts.push(transcript);
    }
    let manifest = json!({
        "delegation_id": delegation_id,
        "parent_session_id": parent_session,
        "created_at": jiff::Timestamp::now().to_string(),
        "tasks": listed,
    });
    std::fs::write(
        dir.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap_or_default(),
    )?;
    Ok(transcripts)
}

/// Removes delegation directories under `root` last modified more than
/// [`KEEP`] before `now`.
pub fn prune(root: &Path, now: SystemTime) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|at| now.duration_since(at).ok())
            .is_some_and(|age| age > KEEP);
        if old && entry.path().is_dir() {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(index: usize) -> TaskEntry {
        TaskEntry {
            task_index: index,
            subagent_id: format!("sa-{index}"),
            goal: "look\naround".to_owned(),
        }
    }

    #[test]
    fn each_task_gets_a_log_and_the_manifest_lists_them() {
        let dir = tempfile::tempdir().unwrap();
        let logs = open(dir.path(), "dg-1", "ses-p", &[task(0), task(1)]).unwrap();
        assert!(
            logs[1]
                .path()
                .ends_with("cache/delegation/live/dg-1/task-2.log")
        );
        logs[0].line("tool read_file README.md");
        let text = std::fs::read_to_string(logs[0].path()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].ends_with("dispatched sa-0 (task 1): look around"));
        assert!(lines[1].ends_with(" tool read_file README.md"));
        let manifest: serde_json::Value = serde_json::from_slice(
            &std::fs::read(dir.path().join("cache/delegation/live/dg-1/manifest.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(manifest["tasks"][1]["log"], "task-2.log");
    }

    #[test]
    fn directories_older_than_seven_days_are_pruned() {
        let dir = tempfile::tempdir().unwrap();
        open(dir.path(), "dg-old", "ses", &[task(0)]).unwrap();
        let root = root(dir.path());
        prune(&root, SystemTime::now() + Duration::from_hours(6 * 24));
        assert!(root.join("dg-old").exists());
        prune(&root, SystemTime::now() + Duration::from_hours(8 * 24));
        assert!(!root.join("dg-old").exists());
    }
}

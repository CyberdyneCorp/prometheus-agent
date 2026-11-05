//! Undo and rollback (spec: execution-environments, File snapshots and
//! undo; Rollback listing, diff and selective restore).
//!
//! Undo puts back what the session's file tools changed in its last turns,
//! refusing a file changed since the session wrote it unless forced.
//! Rollback puts a project back to a checkpoint, leaving files the user
//! edited after the agent's last write as they are unless `all`.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;

use super::checkpoint::{self, Checkpoint};
use super::journal::{self, Edit};
use super::store::{SnapshotStore, digest};

/// What an undo did (or would do).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct UndoReport {
    /// The turns undone.
    pub turns: Vec<u64>,
    /// Files given their prior content.
    pub restored: Vec<PathBuf>,
    /// Files the session created, removed again.
    pub removed: Vec<PathBuf>,
    /// `conflict: <path> changed since turn <n>; use --force to overwrite`.
    pub conflicts: Vec<String>,
}

/// The current content hash of `path` (`None`: missing).
fn current(path: &Path) -> Option<String> {
    fs::read(path).ok().map(|bytes| digest(&bytes))
}

/// Puts `path` back to object `hash` (`None`: removes it).
fn put_back(store: &SnapshotStore, path: &Path, hash: Option<&str>) -> io::Result<()> {
    match hash {
        Some(hash) => {
            let bytes = store.get(hash)?;
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(path, bytes)
        }
        None => match fs::remove_file(path) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => Err(err),
            _ => Ok(()),
        },
    }
}

/// Undoes the file changes of `session`'s last `count` turns up to
/// `current_turn` (`None`: its last journaled turn).
///
/// # Errors
///
/// A file cannot be restored or the journal not written.
pub fn undo(
    store: &SnapshotStore,
    session: &str,
    current_turn: Option<u64>,
    count: u64,
    force: bool,
    dry_run: bool,
) -> io::Result<UndoReport> {
    let edits = journal::live_edits(store, session);
    let last = current_turn.unwrap_or_else(|| edits.iter().map(|e| e.turn).max().unwrap_or(0));
    let from = last.saturating_sub(count.max(1)) + 1;
    let selected: Vec<&Edit> = edits.iter().filter(|e| e.turn >= from).collect();
    let mut by_path: BTreeMap<&Path, (&Edit, &Edit)> = BTreeMap::new();
    for edit in &selected {
        by_path
            .entry(edit.path.as_path())
            .and_modify(|(_, latest)| *latest = edit)
            .or_insert((edit, edit));
    }
    let mut turns: Vec<u64> = selected.iter().map(|e| e.turn).collect();
    turns.dedup();
    let mut report = UndoReport {
        turns,
        ..UndoReport::default()
    };
    for (path, (first, latest)) in by_path {
        if !force && current(path) != latest.after {
            report.conflicts.push(format!(
                "conflict: {} changed since turn {}; use --force to overwrite",
                path.display(),
                latest.turn
            ));
            continue;
        }
        if !dry_run {
            put_back(store, path, first.before.as_deref())?;
        }
        match first.before {
            Some(_) => report.restored.push(path.to_path_buf()),
            None => report.removed.push(path.to_path_buf()),
        }
    }
    if !dry_run && report.conflicts.is_empty() && !report.turns.is_empty() {
        journal::mark_undone(store, session, &report.turns)?;
    }
    Ok(report)
}

/// One row of the rollback listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Listed {
    /// 1 for the newest.
    pub number: usize,
    /// When it was taken, Unix milliseconds.
    pub created_ms: i64,
    /// What triggered it.
    pub tool: String,
    /// Files that differ from the current state.
    pub files_changed: usize,
    /// Lines added plus removed since.
    pub lines_changed: usize,
}

/// The checkpoints of the project holding `cwd`, newest first.
#[must_use]
pub fn checkpoints(store: &SnapshotStore, cwd: &Path) -> Vec<Checkpoint> {
    let root = checkpoint::project_of(cwd, cwd);
    checkpoint::list(store, &SnapshotStore::workspace_id(&root))
}

/// The rollback listing of `checkpoints`.
#[must_use]
pub fn listing(store: &SnapshotStore, checkpoints: &[Checkpoint]) -> Vec<Listed> {
    checkpoints
        .iter()
        .enumerate()
        .map(|(index, cp)| {
            let changes = changes(store, cp);
            Listed {
                number: index + 1,
                created_ms: cp.created_ms,
                tool: cp.tool.clone(),
                files_changed: changes.len(),
                lines_changed: changes.iter().map(Change::lines).sum(),
            }
        })
        .collect()
}

/// A file that differs between a checkpoint and now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// Its path below the project root.
    pub rel: String,
    /// Its absolute path.
    pub path: PathBuf,
    /// Its content at the checkpoint (`None`: it did not exist).
    pub then: Option<String>,
    /// Its content now (`None`: it does not exist).
    pub now: Option<String>,
    then_text: String,
    now_text: String,
}

impl Change {
    /// Lines added plus removed.
    #[must_use]
    pub fn lines(&self) -> usize {
        similar::TextDiff::from_lines(&self.then_text, &self.now_text)
            .iter_all_changes()
            .filter(|c| c.tag() != similar::ChangeTag::Equal)
            .count()
    }

    /// The unified diff from the checkpoint to now.
    #[must_use]
    pub fn unified(&self) -> String {
        similar::TextDiff::from_lines(&self.then_text, &self.now_text)
            .unified_diff()
            .header(&format!("a/{}", self.rel), &format!("b/{}", self.rel))
            .to_string()
    }
}

/// The files that differ between `cp` and the project now (skipped files
/// never count: their content was not kept).
#[must_use]
pub fn changes(store: &SnapshotStore, cp: &Checkpoint) -> Vec<Change> {
    let (now_files, _) = checkpoint::walk(&cp.root, &cp.excluded);
    let mut found = Vec::new();
    let text = |hash: Option<&str>| {
        hash.and_then(|h| store.get(h).ok())
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default()
    };
    for file in &cp.files {
        let Some(then) = &file.hash else { continue };
        let path = cp.root.join(file.rel_os());
        let now = current(&path);
        if now.as_deref() != Some(then) {
            let now_text = fs::read(&path)
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .unwrap_or_default();
            found.push(Change {
                rel: file.path.clone(),
                path,
                then: Some(then.clone()),
                now,
                then_text: text(Some(then)),
                now_text,
            });
        }
    }
    for (rel, path) in now_files {
        if cp.files.iter().all(|f| f.path != rel) {
            let now_text = fs::read(&path)
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .unwrap_or_default();
            found.push(Change {
                rel,
                now: current(&path),
                path,
                then: None,
                then_text: String::new(),
                now_text,
            });
        }
    }
    found
}

/// What a rollback did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RollbackReport {
    /// Files given their checkpoint content.
    pub restored: Vec<String>,
    /// Files created since, removed.
    pub removed: Vec<String>,
    /// Files the user edited after the agent's last write, left alone.
    pub kept: Vec<String>,
    /// Files too large to have been kept, left alone.
    pub skipped: Vec<String>,
}

impl RollbackReport {
    /// The report as lines for a person.
    #[must_use]
    pub fn render(&self) -> String {
        let mut text = String::new();
        for (label, paths) in [
            ("restored", &self.restored),
            ("removed", &self.removed),
            ("kept (hand-edited)", &self.kept),
            ("skipped (too large)", &self.skipped),
        ] {
            for path in paths {
                let _ = writeln!(text, "{label}: {path}");
            }
        }
        if text.is_empty() {
            text.push_str("nothing to restore\n");
        }
        text
    }
}

/// Puts the project of `cp` back to it: every changed file, or only
/// `only` (a path below the root); a file whose content is not what the
/// agent last wrote is kept unless `all`.
///
/// # Errors
///
/// A file cannot be restored.
pub fn rollback(
    store: &SnapshotStore,
    cp: &Checkpoint,
    all: bool,
    only: Option<&str>,
) -> io::Result<RollbackReport> {
    let written = journal::last_written(store);
    let mut report = RollbackReport {
        skipped: cp.skipped().into_iter().map(str::to_owned).collect(),
        ..RollbackReport::default()
    };
    for change in changes(store, cp) {
        if only.is_some_and(|only| only.trim_start_matches("./") != change.rel) {
            continue;
        }
        let hand_edited = written
            .get(&change.path)
            .is_some_and(|(_, after)| *after != change.now);
        if hand_edited && !all {
            report.kept.push(change.rel);
            continue;
        }
        put_back(store, &change.path, change.then.as_deref())?;
        match change.then {
            Some(_) => report.restored.push(change.rel),
            None => report.removed.push(change.rel),
        }
    }
    store.touch_workspace(&cp.root)?;
    Ok(report)
}

/// The unified diff between `cp` and the project now (nothing changes).
#[must_use]
pub fn diff(store: &SnapshotStore, cp: &Checkpoint) -> String {
    let text: String = changes(store, cp).iter().map(Change::unified).collect();
    if text.is_empty() {
        "no changes since this checkpoint\n".to_owned()
    } else {
        text
    }
}

impl checkpoint::FileEntry {
    /// The relative path in this platform's form.
    fn rel_os(&self) -> PathBuf {
        self.path.split('/').collect()
    }
}

//! Terminal workspace checkpoints (spec: execution-environments, Terminal
//! workspace checkpoints).
//!
//! With `files.snapshot.terminal = "workspace"`, a manifest of the session
//! workspace (each file's size, modification time and content hash) is
//! taken before every terminal command, and the prior content of every
//! file is kept in the snapshot store; after the command a second manifest
//! finds what it changed, created or removed, and each change is journaled
//! like a file-tool edit, so `/undo` reverts it too. A file whose size and
//! modification time match the previous manifest is not read again. A
//! workspace larger than `files.snapshot.max_workspace_bytes` (or with
//! more files than a checkpoint records) is not checkpointed: the command
//! runs, and the session is told once.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use super::checkpoint::MAX_FILES;
use super::store::SnapshotStore;
use super::tree::Tree;

/// One file of a manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Its size in bytes.
    pub size: u64,
    /// Its modification time (nanoseconds since the epoch), when the tree
    /// knows it.
    pub modified: Option<i128>,
    /// Its content's object in the store.
    pub hash: String,
}

/// The files of a workspace, by tree path.
pub type Manifest = BTreeMap<PathBuf, Entry>;

/// What a manifest is taken of.
#[derive(Debug, Clone, Copy)]
pub struct Workspace<'a> {
    /// Where prior contents are kept.
    pub store: &'a SnapshotStore,
    /// Where the files are.
    pub tree: &'a dyn Tree,
    /// The workspace root, a path of `tree`.
    pub root: &'a Path,
    /// Directories left out (Prometheus' own home).
    pub excluded: &'a [PathBuf],
    /// `files.snapshot.max_workspace_bytes`.
    pub max_bytes: u64,
}

/// One file a command changed: its prior and new objects (`None`: absent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// The file, a path of the tree.
    pub path: PathBuf,
    /// Its object before the command.
    pub before: Option<String>,
    /// Its object after.
    pub after: Option<String>,
}

/// Why no manifest was taken.
#[derive(Debug)]
enum Skipped {
    /// The workspace is over the size or file-count limit.
    TooLarge,
    /// The store could not keep a prior content.
    Failed(io::Error),
}

/// One session's workspace checkpoints: the manifest of the running
/// commands' start and the last manifest taken.
#[derive(Debug, Default)]
pub struct Checkpoints {
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    /// Terminal commands running now (concurrent ones are diffed together
    /// when the last ends).
    running: usize,
    /// The manifest from before the first of them.
    before: Option<Manifest>,
    /// The newest manifest, whose unchanged files are not read again.
    last: Manifest,
    /// The over-limit notice was given.
    noticed: bool,
    /// A notice for the next command's result.
    notice: Option<String>,
}

impl Checkpoints {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Before a terminal command: the manifest of `workspace`, unless a
    /// command is already running (its manifest covers this one too).
    pub fn before(&self, workspace: &Workspace<'_>) {
        let mut state = self.lock();
        state.running += 1;
        if state.running > 1 {
            return;
        }
        match manifest(workspace, &state.last) {
            Ok(taken) => state.before = Some(taken),
            Err(Skipped::TooLarge) => {
                state.before = None;
                if !state.noticed {
                    state.noticed = true;
                    state.notice = Some(format!(
                        "workspace checkpoints skipped: {} is over files.snapshot.max_workspace_bytes ({} bytes) or {MAX_FILES} files, so /undo will not revert changes terminal commands make there",
                        workspace.root.display(),
                        workspace.max_bytes
                    ));
                }
            }
            Err(Skipped::Failed(err)) => {
                state.before = None;
                tracing::warn!(root = %workspace.root.display(), "workspace checkpoint failed: {err}");
            }
        }
    }

    /// After a terminal command: what the commands that ran since the
    /// first [`Checkpoints::before`] changed (empty while others still
    /// run), and the one-time notice when checkpoints were skipped.
    pub fn after(&self, workspace: &Workspace<'_>) -> (Vec<Change>, Option<String>) {
        let mut state = self.lock();
        state.running = state.running.saturating_sub(1);
        let notice = state.notice.take();
        if state.running > 0 {
            return (Vec::new(), notice);
        }
        let Some(before) = state.before.take() else {
            return (Vec::new(), notice);
        };
        if let Ok(after) = manifest(workspace, &before) {
            let changes = changes(&before, &after);
            state.last = after;
            return (changes, notice);
        }
        tracing::warn!(root = %workspace.root.display(), "workspace changes not recorded: the workspace could not be read again");
        (Vec::new(), notice)
    }
}

/// The manifest of `workspace` now: a file unchanged since `previous`
/// (same size and modification time, its object still stored) keeps its
/// hash; every other file is read and its content stored.
fn manifest(workspace: &Workspace<'_>, previous: &Manifest) -> Result<Manifest, Skipped> {
    let (files, truncated) = workspace
        .tree
        .walk(workspace.root, workspace.excluded, MAX_FILES);
    if truncated {
        return Err(Skipped::TooLarge);
    }
    let mut sized = Vec::with_capacity(files.len());
    let mut total: u64 = 0;
    for path in files.into_values() {
        let Ok(size) = workspace.tree.size(&path) else {
            continue;
        };
        total = total.saturating_add(size);
        if total > workspace.max_bytes {
            return Err(Skipped::TooLarge);
        }
        sized.push((path, size));
    }
    let mut taken = Manifest::new();
    for (path, size) in sized {
        let modified = workspace.tree.modified(&path);
        let known = previous.get(&path).filter(|e| {
            e.size == size
                && modified.is_some()
                && e.modified == modified
                && workspace.store.contains(&e.hash)
        });
        let hash = match known {
            Some(entry) => entry.hash.clone(),
            None => match workspace.tree.read(&path) {
                Ok(bytes) => workspace
                    .store
                    .put_relaxed(&bytes)
                    .map_err(Skipped::Failed)?,
                // Gone or unreadable meanwhile: not part of the workspace.
                Err(_) => continue,
            },
        };
        taken.insert(
            path,
            Entry {
                size,
                modified,
                hash,
            },
        );
    }
    Ok(taken)
}

/// The files whose content differs between `before` and `after`.
fn changes(before: &Manifest, after: &Manifest) -> Vec<Change> {
    let paths: BTreeSet<&PathBuf> = before.keys().chain(after.keys()).collect();
    paths
        .into_iter()
        .filter_map(|path| {
            let was = before.get(path).map(|e| e.hash.clone());
            let now = after.get(path).map(|e| e.hash.clone());
            (was != now).then(|| Change {
                path: path.clone(),
                before: was,
                after: now,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshots::HostTree;

    struct Fixture {
        _dir: tempfile::TempDir,
        store: SnapshotStore,
        root: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("work");
            std::fs::create_dir_all(&root).unwrap();
            let store = SnapshotStore::of_profile(&dir.path().join("profile"));
            Self {
                _dir: dir,
                store,
                root,
            }
        }

        fn workspace(&self, max_bytes: u64) -> Workspace<'_> {
            Workspace {
                store: &self.store,
                tree: &HostTree,
                root: &self.root,
                excluded: &[],
                max_bytes,
            }
        }
    }

    #[test]
    fn a_command_s_edits_creations_and_removals_are_found_with_their_prior_content() {
        let fx = Fixture::new();
        std::fs::write(fx.root.join("notes.txt"), "a\n").unwrap();
        std::fs::write(fx.root.join("old.txt"), "gone soon\n").unwrap();
        std::fs::write(fx.root.join("same.txt"), "kept\n").unwrap();
        let checkpoints = Checkpoints::default();
        let ws = fx.workspace(1 << 20);
        checkpoints.before(&ws);
        std::fs::write(fx.root.join("notes.txt"), "b\n").unwrap();
        std::fs::remove_file(fx.root.join("old.txt")).unwrap();
        std::fs::write(fx.root.join("new.txt"), "made\n").unwrap();
        let (changes, notice) = checkpoints.after(&ws);
        assert_eq!(notice, None);
        let names: Vec<String> = changes
            .iter()
            .map(|c| c.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["new.txt", "notes.txt", "old.txt"]);
        let notes = &changes[1];
        assert_eq!(
            fx.store.get(notes.before.as_ref().unwrap()).unwrap(),
            b"a\n"
        );
        assert_eq!(fx.store.get(notes.after.as_ref().unwrap()).unwrap(), b"b\n");
        assert_eq!(changes[0].before, None);
        assert_eq!(changes[2].after, None);
    }

    #[test]
    fn an_oversized_workspace_is_skipped_with_one_notice() {
        let fx = Fixture::new();
        std::fs::write(fx.root.join("big.bin"), vec![0u8; 64]).unwrap();
        let checkpoints = Checkpoints::default();
        let ws = fx.workspace(10);
        checkpoints.before(&ws);
        std::fs::write(fx.root.join("big.bin"), vec![1u8; 64]).unwrap();
        let (changes, notice) = checkpoints.after(&ws);
        assert!(changes.is_empty());
        assert!(notice.unwrap().contains("max_workspace_bytes"));
        checkpoints.before(&ws);
        assert_eq!(checkpoints.after(&ws), (Vec::new(), None));
    }

    #[test]
    fn concurrent_commands_are_diffed_when_the_last_ends() {
        let fx = Fixture::new();
        std::fs::write(fx.root.join("a.txt"), "1").unwrap();
        let checkpoints = Checkpoints::default();
        let ws = fx.workspace(1 << 20);
        checkpoints.before(&ws);
        checkpoints.before(&ws);
        std::fs::write(fx.root.join("a.txt"), "2").unwrap();
        assert!(checkpoints.after(&ws).0.is_empty());
        assert_eq!(checkpoints.after(&ws).0.len(), 1);
    }
}

//! Checkpoints: a manifest of every file of a project (its `.gitignore`
//! respected, `.git` and nested repositories left out) with each file's
//! content in the object pool. Files larger than the size cap are listed
//! as skipped instead of stored.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::store::{SnapshotStore, now_ms, read_json, write_json};

/// Files recorded per checkpoint at most (the rest is left out and the
/// checkpoint marked `truncated`).
pub const MAX_FILES: usize = 20_000;

/// One file of a checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    /// Its path below the project root, `/`-separated.
    pub path: String,
    /// Its content's object, `None` when it was skipped.
    pub hash: Option<String>,
    /// Its size in bytes.
    pub size: u64,
}

/// One checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    /// Its id (sortable by creation).
    pub id: String,
    /// When it was taken, Unix milliseconds.
    pub created_ms: i64,
    /// The project's root.
    pub root: PathBuf,
    /// The session that took it.
    pub session: String,
    /// That session's turn.
    pub turn: u64,
    /// What triggered it (`write_file`, `patch`, `terminal`).
    pub tool: String,
    /// Every file, in path order.
    pub files: Vec<FileEntry>,
    /// More than [`MAX_FILES`] files: the rest were left out.
    #[serde(default)]
    pub truncated: bool,
    /// Directories never recorded (Prometheus' own home).
    #[serde(default)]
    pub excluded: Vec<PathBuf>,
}

impl Checkpoint {
    /// The files that were too large to store.
    #[must_use]
    pub fn skipped(&self) -> Vec<&str> {
        self.files
            .iter()
            .filter(|f| f.hash.is_none())
            .map(|f| f.path.as_str())
            .collect()
    }
}

/// What a checkpoint is taken for.
#[derive(Debug, Clone)]
pub struct Trigger<'a> {
    /// Directories left out of the walk.
    pub excluded: &'a [PathBuf],
    /// The session.
    pub session: &'a str,
    /// Its turn.
    pub turn: u64,
    /// The triggering tool.
    pub tool: &'a str,
}

/// Takes a checkpoint of the project at `root`, keeping at most
/// `max_checkpoints` of it.
///
/// # Errors
///
/// The store cannot be written.
pub fn take(
    store: &SnapshotStore,
    root: &Path,
    trigger: &Trigger<'_>,
    max_file_bytes: u64,
    max_checkpoints: usize,
) -> io::Result<Checkpoint> {
    let id = store.touch_workspace(root)?;
    let (files, truncated) = walk(root, trigger.excluded);
    let mut entries = Vec::with_capacity(files.len());
    for (rel, path) in files {
        let size = fs::metadata(&path).map_or(0, |m| m.len());
        let hash = if size > max_file_bytes {
            None
        } else {
            match fs::read(&path) {
                Ok(bytes) => Some(store.put(&bytes)?),
                Err(_) => continue,
            }
        };
        entries.push(FileEntry {
            path: rel,
            hash,
            size,
        });
    }
    let checkpoint = Checkpoint {
        id: format!(
            "{:013}-{}",
            now_ms(),
            ulid::Ulid::generate().to_string().to_lowercase()
        ),
        created_ms: now_ms(),
        root: root.to_path_buf(),
        session: trigger.session.to_owned(),
        turn: trigger.turn,
        tool: trigger.tool.to_owned(),
        files: entries,
        truncated,
        excluded: trigger.excluded.to_vec(),
    };
    let dir = store.workspace_dir(&id).join("checkpoints");
    write_json(&dir.join(format!("{}.json", checkpoint.id)), &checkpoint)?;
    for old in list(store, &id).into_iter().skip(max_checkpoints) {
        let _ = fs::remove_file(dir.join(format!("{}.json", old.id)));
    }
    Ok(checkpoint)
}

/// The checkpoints of workspace `id`, newest first.
#[must_use]
pub fn list(store: &SnapshotStore, id: &str) -> Vec<Checkpoint> {
    let Ok(entries) = fs::read_dir(store.workspace_dir(id).join("checkpoints")) else {
        return Vec::new();
    };
    let mut found: Vec<Checkpoint> = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| read_json(&e.path()))
        .collect();
    found.sort_by(|a, b| b.id.cmp(&a.id));
    found
}

/// The files of the project at `root` now (leaving out `excluded`):
/// relative path (`/`-separated) to absolute path, and whether
/// [`MAX_FILES`] cut the walk short.
#[must_use]
pub fn walk(root: &Path, excluded: &[PathBuf]) -> (BTreeMap<String, PathBuf>, bool) {
    let top = root.to_path_buf();
    let excluded = excluded.to_vec();
    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(true)
        .require_git(false)
        .follow_links(false)
        .filter_entry(move |entry| {
            let path = entry.path();
            if entry.file_name() == ".git" || excluded.iter().any(|e| path.starts_with(e)) {
                return false;
            }
            // A nested repository is a project of its own.
            !(path != top
                && entry.file_type().is_some_and(|t| t.is_dir())
                && path.join(".git").exists())
        })
        .build();
    let mut files = BTreeMap::new();
    for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        if files.len() >= MAX_FILES {
            return (files, true);
        }
        if let Ok(rel) = entry.path().strip_prefix(root) {
            let rel = rel.to_string_lossy().replace('\\', "/");
            files.insert(rel, entry.path().to_path_buf());
        }
    }
    (files, false)
}

/// The project a path belongs to: the nearest enclosing directory with a
/// `.git`, not above `workspace` when the path is inside it (then the
/// workspace itself); outside it, the nearest repository or the path's
/// directory.
#[must_use]
pub fn project_of(path: &Path, workspace: &Path) -> PathBuf {
    let start = if path.is_dir() {
        path
    } else {
        path.parent().unwrap_or(path)
    };
    let inside = start.starts_with(workspace);
    for dir in start.ancestors() {
        if dir.join(".git").exists() {
            return dir.to_path_buf();
        }
        if inside && dir == workspace {
            return workspace.to_path_buf();
        }
    }
    start.to_path_buf()
}

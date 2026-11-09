//! A session's file-tool edits (spec: execution-environments, File
//! snapshots and undo): one JSON line per change with the file's prior and
//! new content (objects in the pool, `None` for absence), keyed by
//! session, turn and call. An undo appends a marker; the edits it covered
//! stay in the journal but are no longer undone again.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, BufRead as _, Write as _};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::store::{SnapshotStore, now_ms};

/// One file change by a file tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edit {
    /// The session's turn.
    pub turn: u64,
    /// The model's call id.
    pub call_id: String,
    /// The tool.
    pub tool: String,
    /// The changed file, as its tree names it (a host path, or a path on
    /// `backend`).
    pub path: PathBuf,
    /// The backend whose own filesystem holds `path` (`None`: this host).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    /// Its content before (`None`: it did not exist).
    pub before: Option<String>,
    /// Its content after (`None`: it was removed).
    pub after: Option<String>,
    /// When, Unix milliseconds.
    pub at_ms: i64,
}

/// One journal line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Line {
    Edit(Edit),
    /// Turns undone (their edits are not undone twice).
    Undone {
        turns: Vec<u64>,
        at_ms: i64,
    },
}

/// Appends `edit` to `session`'s journal.
///
/// # Errors
///
/// The journal cannot be written.
pub fn append(store: &SnapshotStore, session: &str, edit: Edit) -> io::Result<()> {
    write_line(store, session, &Line::Edit(edit))
}

/// Marks `turns` of `session` as undone.
///
/// # Errors
///
/// The journal cannot be written.
pub fn mark_undone(store: &SnapshotStore, session: &str, turns: &[u64]) -> io::Result<()> {
    write_line(
        store,
        session,
        &Line::Undone {
            turns: turns.to_vec(),
            at_ms: now_ms(),
        },
    )
}

fn write_line(store: &SnapshotStore, session: &str, line: &Line) -> io::Result<()> {
    store.dir("journals")?;
    let mut text = serde_json::to_string(line).map_err(io::Error::other)?;
    text.push('\n');
    let mut file = pmt_core::private_fs::append_options().open(store.journal_path(session))?;
    file.write_all(text.as_bytes())
}

fn lines(path: &Path) -> Vec<Line> {
    let Ok(file) = fs::File::open(path) else {
        return Vec::new();
    };
    io::BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| serde_json::from_str(&line).ok())
        .collect()
}

/// Every edit of the journal at `path`, oldest first.
#[must_use]
pub fn read_edits(path: &Path) -> Vec<Edit> {
    lines(path)
        .into_iter()
        .filter_map(|line| match line {
            Line::Edit(edit) => Some(edit),
            Line::Undone { .. } => None,
        })
        .collect()
}

/// Every edit of `session`, oldest first, undone ones included (the first
/// edit of each file holds its content before the session).
#[must_use]
pub fn session_edits(store: &SnapshotStore, session: &str) -> Vec<Edit> {
    read_edits(&store.journal_path(session))
}

/// The edits of `session` not yet undone, oldest first.
#[must_use]
pub fn live_edits(store: &SnapshotStore, session: &str) -> Vec<Edit> {
    let mut edits: Vec<Edit> = Vec::new();
    for line in lines(&store.journal_path(session)) {
        match line {
            Line::Edit(edit) => edits.push(edit),
            Line::Undone { turns, .. } => edits.retain(|e| !turns.contains(&e.turn)),
        }
    }
    edits
}

/// The content the agent last wrote to each file of `backend` (`None`:
/// this host), over every journal of the store (`None`: the agent removed
/// it).
#[must_use]
pub fn last_written(
    store: &SnapshotStore,
    backend: Option<&str>,
) -> BTreeMap<PathBuf, (i64, Option<String>)> {
    let mut last: BTreeMap<PathBuf, (i64, Option<String>)> = BTreeMap::new();
    for journal in store.journals() {
        for edit in read_edits(&journal)
            .into_iter()
            .filter(|e| e.backend.as_deref() == backend)
        {
            let newer = last.get(&edit.path).is_none_or(|(at, _)| *at <= edit.at_ms);
            if newer {
                last.insert(edit.path, (edit.at_ms, edit.after));
            }
        }
    }
    last
}

/// The highest turn `session`'s journal names.
#[must_use]
pub fn last_turn(store: &SnapshotStore, session: &str) -> u64 {
    read_edits(&store.journal_path(session))
        .iter()
        .map(|e| e.turn)
        .max()
        .unwrap_or(0)
}

//! `files.undo` and `files.rollback` (spec: execution-environments, File
//! snapshots and undo; Rollback listing, diff and selective restore).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::Method;

/// `files.undo` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FilesUndoParams {
    /// The session.
    pub session_id: String,
    /// How many of its last turns (default 1).
    #[serde(default)]
    pub turns: Option<u64>,
    /// Overwrite files changed since the session wrote them.
    #[serde(default)]
    pub force: bool,
    /// Report only; change nothing.
    #[serde(default)]
    pub dry_run: bool,
}

/// `files.undo` result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FilesUndone {
    /// The turns undone.
    pub turns: Vec<u64>,
    /// Files given their prior content.
    pub restored: Vec<String>,
    /// Files the session had created, removed.
    pub removed: Vec<String>,
    /// `conflict: <path> changed since turn <n>; use --force to overwrite`.
    pub conflicts: Vec<String>,
}

/// `files.rollback` params: no checkpoint lists them; `diff` previews.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FilesRollbackParams {
    /// The session (its workspace).
    pub session_id: String,
    /// The checkpoint, 1 for the newest.
    #[serde(default)]
    pub checkpoint: Option<u32>,
    /// Restore only this file.
    #[serde(default)]
    pub file: Option<String>,
    /// Show the diff to the current state; change nothing.
    #[serde(default)]
    pub diff: bool,
    /// Overwrite hand-edited files too.
    #[serde(default)]
    pub all: bool,
}

/// `files.rollback` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FilesRolledBack {
    /// The listing, the diff or what was restored, as text.
    pub output: String,
}

method!(/// `files.undo`: undoes the file changes of a session's last turns.
    FilesUndo = "files.undo" (FilesUndoParams) -> FilesUndone);
method!(/// `files.rollback`: lists, previews or restores workspace checkpoints.
    FilesRollback = "files.rollback" (FilesRollbackParams) -> FilesRolledBack);

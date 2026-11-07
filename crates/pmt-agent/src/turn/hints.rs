//! Subdirectory hints ride tool results (spec: agent-turn-loop,
//! Subdirectory hints ride tool results).
//!
//! When a file tool reads or writes inside a subdirectory of the session's
//! working tree holding an `AGENTS.md` that was not loaded yet, that
//! file's content (scanned, head+tail truncated past 32,000 characters) is
//! appended to the triggering tool result, never to the system prompt, so
//! the cached prefix is untouched. Each file is added at most once per
//! session: the files the project context already loaded count as loaded,
//! and hints from earlier turns are recognized by their header in the
//! session's stored tool rows. Paths outside the working tree are ignored.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use pmt_store::{Role, Store, Visibility};
use pmt_tools::{Content, Status};
use serde_json::Value;

use super::model::RequestedCall;
use super::observe::edited_paths;
use super::run::Turn;
use super::tools::ToolOutcome;
use crate::prompt::context;

/// Characters a hint keeps.
pub(super) const HINT_MAX_CHARS: usize = 32_000;

/// The header line of a hint; the path is relative to the working tree.
const HEADER: &str = "[Subdirectory context: ";

/// The `AGENTS.md` files of a session already in its context.
#[derive(Debug, Default)]
pub(super) struct Hints {
    /// `None` until the first file tool call of this process.
    loaded: Mutex<Option<HashSet<PathBuf>>>,
}

/// The working tree of `cwd`: its git root, else `cwd`.
fn tree(cwd: &Path) -> PathBuf {
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    context::git_root(&cwd).unwrap_or(cwd)
}

/// The paths a file tool call names.
fn touched(tool: &str, arguments: &str) -> Vec<String> {
    match tool {
        "write_file" | "patch" => edited_paths(tool, arguments),
        "read_file" | "search_files" => serde_json::from_str::<Value>(arguments)
            .ok()
            .and_then(|args| args.get("path")?.as_str().map(str::to_owned))
            .into_iter()
            .collect(),
        _ => Vec::new(),
    }
}

/// The directory `path` (relative to `cwd`) puts the agent in, inside
/// `root`; `None` outside it.
fn directory(cwd: &Path, root: &Path, path: &str) -> Option<PathBuf> {
    let joined = cwd.join(path);
    let resolved = joined.canonicalize().ok().or_else(|| {
        let parent = joined.parent()?.canonicalize().ok()?;
        Some(parent.join(joined.file_name()?))
    })?;
    let dir = if resolved.is_dir() {
        resolved
    } else {
        resolved.parent()?.to_path_buf()
    };
    dir.starts_with(root).then_some(dir)
}

/// The `AGENTS.md` files from `root` down to `dir`, outermost first.
fn agents_files(root: &Path, dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = dir
        .ancestors()
        .take_while(|d| d.starts_with(root))
        .map(|d| d.join("AGENTS.md"))
        .filter(|f| f.is_file())
        .collect();
    files.reverse();
    files
}

/// The files already in the session's context: the project-context chain
/// from `root` to `cwd`, and the hints stored in earlier tool rows.
fn seed(
    store: &Store,
    session: pmt_core::id::SessionId,
    root: &Path,
    cwd: &Path,
) -> HashSet<PathBuf> {
    let mut loaded: HashSet<PathBuf> = agents_files(root, cwd).into_iter().collect();
    let rows = store.messages(session, Visibility::All).unwrap_or_default();
    for row in rows.iter().filter(|r| r.role == Role::Tool) {
        let text = row.content.as_deref().unwrap_or_default();
        for line in text.lines() {
            if let Some(rel) = line.strip_prefix(HEADER).and_then(|l| l.strip_suffix(']')) {
                loaded.insert(root.join(rel));
            }
        }
    }
    loaded
}

/// The hint block of one file.
fn hint(root: &Path, file: &Path) -> Option<String> {
    let text = std::fs::read_to_string(file).ok()?;
    let rel = file
        .strip_prefix(root)
        .unwrap_or(file)
        .display()
        .to_string();
    let body = context::prepare(&rel, &text, HINT_MAX_CHARS);
    Some(format!("{HEADER}{rel}]\n{body}"))
}

impl Turn<'_> {
    /// Appends each not-yet-loaded subdirectory `AGENTS.md` to the result
    /// of the call that first reached it.
    pub(super) fn subdirectory_hints(&self, calls: &[RequestedCall], outcomes: &mut [ToolOutcome]) {
        let setup = &self.agent.setup;
        let Some(cwd) = setup.prompt.cwd.as_deref() else {
            return;
        };
        if !self.config.prompt.load_context_files || setup.prompt.no_context_files {
            return;
        }
        let root = tree(cwd);
        let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
        let mut guard = self
            .agent
            .hints
            .loaded
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let loaded = guard.get_or_insert_with(|| seed(&setup.store, setup.session, &root, &cwd));
        let place = Place {
            cwd: &cwd,
            root: &root,
        };
        for (call, outcome) in calls.iter().zip(outcomes.iter_mut()) {
            if matches!(outcome.result.status, Status::Ok | Status::Degraded) {
                let texts = place.new_hints(call, loaded);
                outcome
                    .result
                    .content
                    .extend(texts.into_iter().map(|text| Content::Text { text }));
            }
        }
    }
}

/// Where a session's file tools resolve paths.
struct Place<'a> {
    cwd: &'a Path,
    root: &'a Path,
}

impl Place<'_> {
    /// The hints `call` brings that are not in `loaded` yet (now added).
    fn new_hints(&self, call: &RequestedCall, loaded: &mut HashSet<PathBuf>) -> Vec<String> {
        touched(&call.name, &call.arguments)
            .iter()
            .filter_map(|path| directory(self.cwd, self.root, path))
            .flat_map(|dir| agents_files(self.root, &dir))
            .filter(|file| loaded.insert(file.clone()))
            .filter_map(|file| hint(self.root, &file))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_tools_name_their_paths() {
        assert_eq!(
            touched("read_file", r#"{"path":"pkg/sub/lib.rs"}"#),
            ["pkg/sub/lib.rs"]
        );
        assert_eq!(
            touched("search_files", r#"{"pattern":"x","path":"pkg"}"#),
            ["pkg"]
        );
        assert!(touched("terminal", r#"{"command":"cat pkg/sub/lib.rs"}"#).is_empty());
    }

    #[test]
    fn paths_outside_the_tree_are_ignored() {
        let root = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let tree = root.path().canonicalize().unwrap();
        let outside = other.path().join("x.rs");
        assert_eq!(directory(&tree, &tree, outside.to_str().unwrap()), None);
        assert_eq!(directory(&tree, &tree, "../escape.rs"), None);
    }
}

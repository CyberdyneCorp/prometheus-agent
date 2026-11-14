//! Edit approvals of editor sessions (spec: acp-adapter, Edit-approval
//! modes): an ACP session asks before file edits (`default`), lets edits
//! inside its workspace through (`accept_edits`) or never asks
//! (`dont_ask`). The card the editor shows is a diff, so each asked edit
//! carries the file before and after ([`previews`]), computed from the
//! call's arguments without touching the file.

use std::path::Path;

use serde_json::Value;

use super::gate::EditDiff;
use crate::files::patch::PatchArgs;
use crate::files::patch::v4a::{self, Op};

/// The detector name of an edit an editor session asks about.
pub const FILE_EDIT: &str = "file-edit";

/// The largest file whose content a diff card shows.
const MAX_SHOWN: u64 = 512 * 1024;

/// How an editor session's file edits are approved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditPolicy {
    /// Ask before every edit.
    Ask,
    /// Edits inside the session's working directory run without asking.
    AcceptInWorkspace,
    /// Every edit runs without asking.
    DontAsk,
}

impl EditPolicy {
    /// Whether writing `path` (absolute) asks, for a session working in
    /// `workspace`.
    #[must_use]
    pub fn asks(self, path: &Path, workspace: &Path) -> bool {
        match self {
            Self::Ask => true,
            Self::AcceptInWorkspace => !path.starts_with(workspace),
            Self::DontAsk => false,
        }
    }
}

/// The text of `path` as a diff card shows it: `None` for a missing,
/// binary or oversized file.
fn current(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_SHOWN {
        return None;
    }
    String::from_utf8(std::fs::read(path).ok()?).ok()
}

/// The diff of writing `path` (absolute) as `new_text`.
fn whole(path: &Path, new_text: String) -> EditDiff {
    EditDiff {
        path: path.to_string_lossy().into_owned(),
        old_text: current(path),
        new_text,
    }
}

/// What a `replace` patch makes of the file: the exact replacement when
/// `old_string` is found, else the two fragments themselves (the tool's
/// fuzzy matching may still find them).
fn replaced(path: &Path, old_string: &str, new_string: &str, all: bool) -> EditDiff {
    match current(path) {
        Some(text) if text.contains(old_string) => {
            let new_text = if all {
                text.replace(old_string, new_string)
            } else {
                text.replacen(old_string, new_string, 1)
            };
            EditDiff {
                path: path.to_string_lossy().into_owned(),
                old_text: Some(text),
                new_text,
            }
        }
        _ => EditDiff {
            path: path.to_string_lossy().into_owned(),
            old_text: Some(old_string.to_owned()),
            new_text: new_string.to_owned(),
        },
    }
}

/// The diffs of a V4A patch, one per file it touches.
fn patched(text: &str, resolve: &dyn Fn(&str) -> std::path::PathBuf) -> Vec<EditDiff> {
    let Ok(ops) = v4a::parse(text) else {
        return Vec::new();
    };
    ops.iter()
        .map(|op| {
            let path = resolve(op.path());
            match op {
                Op::Add { content, .. } => whole(&path, content.clone()),
                Op::Delete { .. } => whole(&path, String::new()),
                Op::Update { hunks, .. } => {
                    let old = current(&path).unwrap_or_default();
                    let new_text = v4a::apply_hunks(&old, hunks).unwrap_or_else(|_| old.clone());
                    EditDiff {
                        path: path.to_string_lossy().into_owned(),
                        old_text: Some(old),
                        new_text,
                    }
                }
            }
        })
        .collect()
}

/// The diffs of a file tool call (`write_file`, `patch`); empty for any
/// other tool. `resolve` turns an argument path into an absolute one.
#[must_use]
pub fn previews(
    tool: &str,
    arguments: &Value,
    resolve: &dyn Fn(&str) -> std::path::PathBuf,
) -> Vec<EditDiff> {
    let text = |key: &str| arguments.get(key).and_then(Value::as_str);
    match tool {
        "write_file" => text("path")
            .map(|path| {
                whole(
                    &resolve(path),
                    text("content").unwrap_or_default().to_owned(),
                )
            })
            .into_iter()
            .collect(),
        "patch" => match PatchArgs::from_value(arguments) {
            Ok(PatchArgs::Replace {
                path,
                old_string,
                new_string,
                replace_all,
            }) => vec![replaced(
                &resolve(&path),
                &old_string,
                &new_string,
                replace_all,
            )],
            Ok(PatchArgs::V4a { patch }) => patched(&patch, resolve),
            Err(_) => Vec::new(),
        },
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use serde_json::json;

    use super::{EditPolicy, previews};

    #[test]
    fn accept_edits_asks_only_outside_the_workspace() {
        let workspace = Path::new("/work/app");
        let inside = Path::new("/work/app/src/main.rs");
        let outside = Path::new("/etc/hosts");
        assert!(!EditPolicy::AcceptInWorkspace.asks(inside, workspace));
        assert!(EditPolicy::AcceptInWorkspace.asks(outside, workspace));
        assert!(EditPolicy::Ask.asks(inside, workspace));
        assert!(!EditPolicy::DontAsk.asks(outside, workspace));
    }

    #[test]
    fn a_replace_patch_previews_the_whole_file_before_and_after() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        let root = dir.path().to_path_buf();
        let resolve = move |p: &str| -> PathBuf { root.join(p) };
        let args = json!({"path": "a.txt", "old_string": "two", "new_string": "three"});
        let diffs = previews("patch", &args, &resolve);
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].old_text.as_deref(), Some("one\ntwo\n"));
        assert_eq!(diffs[0].new_text, "one\nthree\n");
        let new = previews(
            "write_file",
            &json!({"path": "b.txt", "content": "x"}),
            &resolve,
        );
        assert_eq!(new[0].old_text, None);
        assert_eq!(new[0].new_text, "x");
        assert!(previews("terminal", &json!({"command": "ls"}), &resolve).is_empty());
    }
}

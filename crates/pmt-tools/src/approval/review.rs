//! Inline diff review of file edits (spec: tui, Inline diff review for
//! file edits): the numbered hunks of an edit an approval asks about, the
//! file with only the accepted hunks applied (or the person's own edited
//! version), and the call rewritten so the tool writes exactly that. The
//! tool's result then says which hunks were applied and which rejected.

use std::path::Path;

use serde_json::Value;
use similar::{ChangeTag, DiffOp, DiffTag, TextDiff};

use super::gate::EditDiff;
use crate::files::patch::PatchArgs;
use crate::files::patch::v4a::{self, Hunk as V4aHunk, Line, Op};

/// Unchanged lines shown around each change; changes closer than twice
/// this share a hunk.
const CONTEXT: usize = 2;

/// One numbered hunk of an edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// `@@ -<old start>,<old lines> +<new start>,<new lines> @@`.
    pub header: String,
    /// Its lines, each starting with ` `, `-` or `+`.
    pub lines: Vec<String>,
}

/// What the person answered about an edit's content.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EditReview {
    /// The hunks to apply (numbered from 1); `None`: all of them.
    pub accepted_hunks: Option<Vec<u32>>,
    /// The file content to write instead of the proposed one.
    pub replacement: Option<String>,
}

impl EditReview {
    /// Whether it changes what the call writes.
    #[must_use]
    pub fn revises(&self) -> bool {
        self.accepted_hunks.is_some() || self.replacement.is_some()
    }
}

/// A call rewritten after a review.
#[derive(Debug, Clone, PartialEq)]
pub struct Revision {
    /// The arguments the tool runs with.
    pub arguments: Value,
    /// What the tool's result tells the model about the review.
    pub note: String,
}

fn diff_of<'a>(old: &'a str, new: &'a str) -> TextDiff<'a, 'a, str> {
    TextDiff::from_lines(old, new)
}

/// The numbered hunks (in order, the first is hunk 1) of changing `old`
/// into `new`.
#[must_use]
pub fn hunks(old: &str, new: &str) -> Vec<Hunk> {
    let diff = diff_of(old, new);
    diff.grouped_ops(CONTEXT)
        .iter()
        .filter(|group| !group.is_empty())
        .map(|group| {
            let (first, last) = (&group[0], &group[group.len() - 1]);
            let old_start = first.old_range().start;
            let new_start = first.new_range().start;
            let old_len = last.old_range().end - old_start;
            let new_len = last.new_range().end - new_start;
            let header = format!(
                "@@ -{},{old_len} +{},{new_len} @@",
                old_start + 1,
                new_start + 1
            );
            let lines = group
                .iter()
                .flat_map(|op| diff.iter_changes(op))
                .map(|change| {
                    let sign = match change.tag() {
                        ChangeTag::Equal => ' ',
                        ChangeTag::Delete => '-',
                        ChangeTag::Insert => '+',
                    };
                    let text = change.value();
                    format!("{sign}{}", text.strip_suffix('\n').unwrap_or(text))
                })
                .collect();
            Hunk { header, lines }
        })
        .collect()
}

/// `old` with only the `accepted` hunks (numbered as [`hunks`] numbers
/// them) of the change to `new` applied.
#[must_use]
pub fn merge(old: &str, new: &str, accepted: &[u32]) -> String {
    let diff = diff_of(old, new);
    let groups = diff.grouped_ops(CONTEXT);
    let number = |op: &DiffOp| {
        groups
            .iter()
            .position(|group| group.contains(op))
            .and_then(|n| u32::try_from(n + 1).ok())
    };
    let mut merged = String::with_capacity(new.len().max(old.len()));
    for op in diff.ops() {
        let take_new =
            op.tag() != DiffTag::Equal && number(op).is_some_and(|n| accepted.contains(&n));
        let lines: Vec<&str> = if take_new {
            op.new_range().filter_map(|i| diff.new_slice(i)).collect()
        } else {
            op.old_range().filter_map(|i| diff.old_slice(i)).collect()
        };
        merged.extend(lines);
    }
    merged
}

/// `numbers` as `1, 3`.
fn listed(numbers: &[u32]) -> String {
    numbers
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// What the model is told about a review of the edit to `path`.
fn note(path: &str, total: u32, review: &EditReview) -> String {
    if review.replacement.is_some() {
        return format!(
            "Edit review of {path}: the user edited the proposed change before accepting it; the file was written with the user's version."
        );
    }
    let accepted = review.accepted_hunks.as_deref().unwrap_or_default();
    let applied: Vec<u32> = (1..=total).filter(|n| accepted.contains(n)).collect();
    let rejected: Vec<u32> = (1..=total).filter(|n| !accepted.contains(n)).collect();
    let rejected = match rejected.as_slice() {
        [] => String::new(),
        [one] => format!("; hunk {one} was rejected by the user"),
        many => format!("; hunks {} were rejected by the user", listed(many)),
    };
    format!(
        "Edit review of {path}: applied {} {}{rejected}. The file contains only the applied changes.",
        if applied.len() == 1 { "hunk" } else { "hunks" },
        listed(&applied)
    )
}

/// Rewrites a `write_file` or `patch` call so that it writes the reviewed
/// content of `diff`'s file. `resolve` turns an argument path into the
/// absolute path diffs carry.
///
/// # Errors
///
/// Every hunk rejected (nothing to write), or a call this review cannot
/// be applied to.
pub fn revise(
    tool: &str,
    arguments: &Value,
    diff: &EditDiff,
    review: &EditReview,
    resolve: &dyn Fn(&str) -> std::path::PathBuf,
) -> Result<Revision, String> {
    let old = diff.old_text.as_deref().unwrap_or_default();
    let total = u32::try_from(hunks(old, &diff.new_text).len()).unwrap_or(u32::MAX);
    let content = match (&review.replacement, &review.accepted_hunks) {
        (Some(replacement), _) => replacement.clone(),
        (None, Some(accepted)) if !(1..=total).any(|n| accepted.contains(&n)) => {
            return Err(format!(
                "the user rejected every hunk of the edit to {}",
                diff.path
            ));
        }
        (None, Some(accepted)) => merge(old, &diff.new_text, accepted),
        (None, None) => diff.new_text.clone(),
    };
    let mut arguments = arguments.clone();
    match tool {
        "write_file" => arguments["content"] = Value::String(content),
        "patch" => patch_arguments(&mut arguments, diff, content, resolve)?,
        other => return Err(format!("{other} edits cannot be reviewed hunk by hunk")),
    }
    Ok(Revision {
        arguments,
        note: note(&diff.path, total, review),
    })
}

fn patch_arguments(
    arguments: &mut Value,
    diff: &EditDiff,
    content: String,
    resolve: &dyn Fn(&str) -> std::path::PathBuf,
) -> Result<(), String> {
    match PatchArgs::from_value(arguments)? {
        PatchArgs::Replace { .. } => {
            // The diff's old side is what the patch replaces: the whole
            // file, or the fragment when the file did not contain it.
            let old = diff.old_text.clone().unwrap_or_default();
            arguments["old_string"] = Value::String(old);
            arguments["new_string"] = Value::String(content);
            arguments["replace_all"] = Value::Bool(false);
        }
        PatchArgs::V4a { patch } => {
            let mut ops = v4a::parse(&patch)?;
            let target = Path::new(&diff.path);
            let op = ops
                .iter_mut()
                .find(|op| resolve(op.path()) == target)
                .ok_or_else(|| format!("the patch does not touch {}", diff.path))?;
            *op = whole_file(op, diff.old_text.as_deref(), content);
            arguments["patch"] = Value::String(v4a::render(&ops));
        }
    }
    Ok(())
}

/// `op` rewritten to turn the file's `old` text into `content`.
fn whole_file(op: &Op, old: Option<&str>, content: String) -> Op {
    let path = op.path().to_owned();
    let Some(old) = old.filter(|_| !matches!(op, Op::Add { .. })) else {
        return Op::Add { path, content };
    };
    let move_to = match op {
        Op::Update { move_to, .. } => move_to.clone(),
        _ => None,
    };
    let mut lines: Vec<Line> = old.lines().map(|l| Line::Remove(l.to_owned())).collect();
    lines.extend(content.lines().map(|l| Line::Add(l.to_owned())));
    Op::Update {
        path,
        move_to,
        hunks: vec![V4aHunk {
            header: None,
            lines,
        }],
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{EditReview, hunks, merge, revise};
    use crate::approval::gate::EditDiff;

    /// Changes at lines 2, 10 and 18 are far enough apart for three hunks.
    const OLD: &str = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm\nn\no\np\nq\nr\ns\nt\n";

    fn edited() -> String {
        OLD.replace("b\n", "B\n")
            .replace("j\n", "J\n")
            .replace("r\n", "R\n")
    }

    #[test]
    fn three_distant_changes_are_three_numbered_hunks() {
        let found = hunks(OLD, &edited());
        assert_eq!(found.len(), 3, "{found:?}");
        assert_eq!(found[0].header, "@@ -1,4 +1,4 @@");
        assert!(found[1].lines.contains(&"-j".to_owned()));
        assert!(found[1].lines.contains(&"+J".to_owned()));
    }

    #[test]
    fn only_the_accepted_hunks_are_applied() {
        let merged = merge(OLD, &edited(), &[1, 3]);
        assert_eq!(merged, OLD.replace("b\n", "B\n").replace("r\n", "R\n"));
        assert_eq!(merge(OLD, &edited(), &[]), OLD);
        assert_eq!(merge(OLD, &edited(), &[1, 2, 3]), edited());
    }

    #[test]
    fn a_reviewed_patch_writes_the_merged_file_and_names_the_rejected_hunk() {
        let diff = EditDiff {
            path: "/w/a.txt".into(),
            old_text: Some(OLD.into()),
            new_text: edited(),
        };
        let review = EditReview {
            accepted_hunks: Some(vec![1, 3]),
            replacement: None,
        };
        let resolve = |p: &str| std::path::Path::new("/w").join(p);
        let args = json!({"path": "a.txt", "old_string": "x", "new_string": "y"});
        let revised = revise("patch", &args, &diff, &review, &resolve).unwrap();
        assert_eq!(revised.arguments["old_string"], OLD);
        assert_eq!(
            revised.arguments["new_string"],
            OLD.replace("b\n", "B\n").replace("r\n", "R\n")
        );
        assert!(
            revised
                .note
                .contains("applied hunks 1, 3; hunk 2 was rejected by the user"),
            "{}",
            revised.note
        );
        let none = EditReview {
            accepted_hunks: Some(vec![]),
            replacement: None,
        };
        assert!(revise("patch", &args, &diff, &none, &resolve).is_err());
    }

    #[test]
    fn an_edited_version_is_written_instead_and_the_model_is_told() {
        let diff = EditDiff {
            path: "/w/AGENTS.md".into(),
            old_text: Some(OLD.into()),
            new_text: edited(),
        };
        let review = EditReview {
            accepted_hunks: None,
            replacement: Some("mine\n".into()),
        };
        let resolve = |p: &str| std::path::Path::new("/w").join(p);
        let args = json!({"path": "AGENTS.md", "content": edited()});
        let revised = revise("write_file", &args, &diff, &review, &resolve).unwrap();
        assert_eq!(revised.arguments["content"], "mine\n");
        assert!(revised.note.contains("written with the user's version"));
    }

    #[test]
    fn a_reviewed_v4a_patch_rewrites_only_the_reviewed_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), OLD).unwrap();
        let root = dir.path().to_path_buf();
        let resolve = move |p: &str| root.join(p);
        let patch = "*** Begin Patch\n*** Update File: a.txt\n@@\n-b\n+B\n@@\n-j\n+J\n*** Add File: n.txt\n+new\n*** End Patch";
        let diff = EditDiff {
            path: dir.path().join("a.txt").to_string_lossy().into_owned(),
            old_text: Some(OLD.into()),
            new_text: OLD.replace("b\n", "B\n").replace("j\n", "J\n"),
        };
        let review = EditReview {
            accepted_hunks: Some(vec![2]),
            replacement: None,
        };
        let revised = revise("patch", &json!({"patch": patch}), &diff, &review, &resolve).unwrap();
        let text = revised.arguments["patch"].as_str().unwrap();
        let ops = crate::files::patch::v4a::parse(text).unwrap();
        assert_eq!(ops.len(), 2);
        let crate::files::patch::v4a::Op::Update { hunks, .. } = &ops[0] else {
            panic!("{ops:?}");
        };
        let applied = crate::files::patch::v4a::apply_hunks(OLD, hunks).unwrap();
        assert_eq!(applied, OLD.replace("j\n", "J\n"));
    }
}

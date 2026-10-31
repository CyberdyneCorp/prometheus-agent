//! `patch` (spec: execution-environments, patch tool contract).
//!
//! Two argument shapes: `mode = "replace"` (default) with `path`,
//! `old_string`, `new_string` and `replace_all`, located by the
//! [`fuzzy`] strategies; and `mode = "v4a"` with a [`v4a`] `patch`
//! applied to all its files or none. The handler accepts either shape
//! whatever the advertised schema said.

pub mod fuzzy;
pub mod v4a;

use std::fmt::Write as _;
use std::path::PathBuf;

use serde_json::{Map, Value, json};

use super::write::WriteTarget;
use super::{FileSession, diff, io_failure, json_result, lines};
use crate::result::{Status, ToolResult};

/// `patch` arguments in either shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchArgs {
    /// Replace `old_string` with `new_string` in `path`.
    Replace {
        /// The file.
        path: String,
        /// Text to find.
        old_string: String,
        /// Its replacement.
        new_string: String,
        /// Replace every match instead of requiring exactly one.
        replace_all: bool,
    },
    /// A V4A multi-file patch.
    V4a {
        /// The patch text.
        patch: String,
    },
}

impl PatchArgs {
    /// Reads either shape: `mode` decides when given, otherwise a `patch`
    /// string without `old_string` means V4A.
    ///
    /// # Errors
    ///
    /// The missing or ill-typed argument.
    pub fn from_value(args: &Value) -> Result<Self, String> {
        let text = |key: &str| args.get(key).and_then(Value::as_str);
        let mode =
            text("mode").unwrap_or(if text("patch").is_some() && text("old_string").is_none() {
                "v4a"
            } else {
                "replace"
            });
        let required = |key: &str| {
            text(key)
                .map(str::to_owned)
                .ok_or_else(|| format!("mode '{mode}' requires the string argument '{key}'"))
        };
        match mode {
            "v4a" => Ok(Self::V4a {
                patch: required("patch")?,
            }),
            "replace" => Ok(Self::Replace {
                path: required("path")?,
                old_string: required("old_string")?,
                new_string: required("new_string")?,
                replace_all: args
                    .get("replace_all")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            }),
            other => Err(format!(
                "unknown patch mode '{other}' (expected 'replace' or 'v4a')"
            )),
        }
    }
}

/// One file of a planned V4A change.
struct Planned {
    shown: String,
    path: PathBuf,
    old: Option<String>,
    new: Option<String>,
}

impl FileSession {
    /// Applies a patch.
    #[must_use]
    pub fn patch(&self, args: &PatchArgs) -> ToolResult {
        match args {
            PatchArgs::Replace {
                path,
                old_string,
                new_string,
                replace_all,
            } => self.replace(path, old_string, new_string, *replace_all),
            PatchArgs::V4a { patch } => self.v4a(patch),
        }
    }

    fn replace(&self, raw: &str, old: &str, new: &str, replace_all: bool) -> ToolResult {
        if old.is_empty() {
            return ToolResult::error("old_string must not be empty");
        }
        if old == new {
            return ToolResult::error("old_string and new_string are identical; nothing to change");
        }
        let (target, content) = match self.existing_text(raw) {
            Ok(found) => found,
            Err(message) => return ToolResult::error(message),
        };
        let Some(found) = fuzzy::find(&content, old) else {
            return ToolResult::error(no_match(raw, &content, old));
        };
        let count = found.spans.len();
        if count > 1 && !replace_all {
            return ToolResult::error(format!(
                "old_string matches {count} locations; add context or set replace_all=true"
            ));
        }
        let mut updated = content.clone();
        let line_based = !matches!(found.strategy, "exact" | "escape_normalized");
        for &(start, end) in found.spans.iter().rev() {
            let replacement = if line_based {
                reindent(new, indent(&content[start..end]), indent(old))
            } else {
                new.to_owned()
            };
            updated.replace_range(start..end, &replacement);
        }
        if let Err(err) = self.backend().write(&target.path, updated.as_bytes()) {
            return io_failure(raw, &err);
        }
        self.note_written(&target.path, &updated);
        let mut body = Map::new();
        body.insert("path".into(), json!(raw));
        body.insert("replacements".into(), json!(count));
        if found.strategy != "exact" {
            body.insert("match_strategy".into(), json!(found.strategy));
        }
        body.insert(
            "diff".into(),
            json!(diff::unified(raw, Some(&content), Some(&updated))),
        );
        body.insert(
            "lint".into(),
            json!(self.setup.linter.check(&target.path, &updated)),
        );
        json_result(Status::Ok, &Value::Object(body))
    }

    /// The guarded write target of an existing UTF-8 file and its text.
    fn existing_text(&self, raw: &str) -> Result<(WriteTarget, String), String> {
        let target = self
            .write_target(raw)
            .map_err(|result| result.text_content())?;
        let Some(bytes) = &target.existing else {
            return Err(format!("File not found: {raw}"));
        };
        let text = String::from_utf8(bytes.clone())
            .map_err(|_| format!("cannot patch '{raw}': it is not UTF-8 text"))?;
        Ok((target, text))
    }

    fn v4a(&self, text: &str) -> ToolResult {
        let ops = match v4a::parse(text) {
            Ok(ops) => ops,
            Err(why) => return ToolResult::error(format!("invalid patch: {why}")),
        };
        let mut planned = Vec::new();
        for op in &ops {
            match self.plan(op) {
                Ok(files) => planned.extend(files),
                Err(why) => {
                    return ToolResult::error(format!(
                        "patch not applied (no file was changed): {}: {why}",
                        op.path()
                    ));
                }
            }
        }
        if let Err(why) = self.commit(&planned) {
            return ToolResult::error(format!("patch not applied (no file was changed): {why}"));
        }
        let mut diffs = String::new();
        let mut files = Vec::new();
        for file in &planned {
            diffs.push_str(&diff::unified(
                &file.shown,
                file.old.as_deref(),
                file.new.as_deref(),
            ));
            let lint = file
                .new
                .as_deref()
                .map(|new| self.setup.linter.check(&file.path, new));
            let action = match (&file.old, &file.new) {
                (None, _) => "added",
                (_, None) => "deleted",
                _ => "updated",
            };
            files.push(json!({"path": file.shown, "action": action, "lint": lint}));
        }
        json_result(Status::Ok, &json!({"files": files, "diff": diffs}))
    }

    /// The files one operation changes, computed without writing.
    fn plan(&self, op: &v4a::Op) -> Result<Vec<Planned>, String> {
        match op {
            v4a::Op::Add { path, content } => {
                let target = self.write_target(path).map_err(|r| r.text_content())?;
                if target.existing.is_some() {
                    return Err("the file already exists; use '*** Update File:'".to_owned());
                }
                Ok(vec![planned(
                    path,
                    target.path,
                    None,
                    Some(content.clone()),
                )])
            }
            v4a::Op::Delete { path } => {
                let (target, old) = self.existing_text(path)?;
                Ok(vec![planned(path, target.path, Some(old), None)])
            }
            v4a::Op::Update {
                path,
                move_to,
                hunks,
            } => {
                let (target, old) = self.existing_text(path)?;
                let new = v4a::apply_hunks(&old, hunks)?;
                let Some(destination) = move_to else {
                    return Ok(vec![planned(path, target.path, Some(old), Some(new))]);
                };
                let moved = self
                    .write_target(destination)
                    .map_err(|r| r.text_content())?;
                if moved.existing.is_some() {
                    return Err(format!("cannot move to '{destination}': it already exists"));
                }
                Ok(vec![
                    planned(path, target.path, Some(old), None),
                    planned(destination, moved.path, None, Some(new)),
                ])
            }
        }
    }

    /// Writes every planned file; on a failure restores the ones already
    /// written, so the patch lands whole or not at all.
    fn commit(&self, planned: &[Planned]) -> Result<(), String> {
        for (index, file) in planned.iter().enumerate() {
            let outcome = match &file.new {
                Some(new) => self.backend().write(&file.path, new.as_bytes()),
                None => self.backend().remove(&file.path),
            };
            if let Err(err) = outcome {
                for done in planned[..index].iter().rev() {
                    let _ = match &done.old {
                        Some(old) => self.backend().write(&done.path, old.as_bytes()),
                        None => self.backend().remove(&done.path),
                    };
                }
                return Err(format!("{}: {err}", file.shown));
            }
        }
        for file in planned {
            if let Some(new) = &file.new {
                self.note_written(&file.path, new);
            }
        }
        Ok(())
    }
}

fn planned(shown: &str, path: PathBuf, old: Option<String>, new: Option<String>) -> Planned {
    Planned {
        shown: shown.to_owned(),
        path,
        old,
        new,
    }
}

/// Leading whitespace of the first line of `text`.
fn indent(text: &str) -> &str {
    let first = text.lines().next().unwrap_or_default();
    &first[..first.len() - first.trim_start().len()]
}

/// A whole-line fuzzy match keeps the file's indentation: when the matched
/// lines are indented deeper than `old_string`, `new_string` is indented by
/// the same extra amount.
fn reindent(new: &str, matched: &str, old: &str) -> String {
    let Some(extra) = matched.strip_prefix(old).filter(|extra| !extra.is_empty()) else {
        return new.to_owned();
    };
    new.split('\n')
        .map(|line| {
            if line.trim().is_empty() {
                line.to_owned()
            } else {
                format!("{extra}{line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `old_string not found` with the closest candidate region.
fn no_match(raw: &str, content: &str, old: &str) -> String {
    let mut message = format!("old_string not found in '{raw}'");
    if let Some((first, last, similarity)) = fuzzy::closest(content, old) {
        let all = lines(content);
        let region: Vec<String> = (first..=last)
            .filter_map(|n| all.get(n - 1).map(|line| format!("{n:>6}|{line}")))
            .collect();
        let percent = (similarity * 100.0).round();
        let _ = write!(
            message,
            "\nClosest candidate (lines {first}-{last}, {percent}% similar):\n{}",
            region.join("\n")
        );
    }
    message
}

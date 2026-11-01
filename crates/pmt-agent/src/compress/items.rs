//! What a summary must not lose, extracted deterministically from the
//! summarized region, and the fallback summary built from it (spec:
//! context-compression, Summary failure handling; the coverage check that
//! uses the same items is the M2 quality upgrade).

use std::collections::HashSet;
use std::sync::LazyLock;

use pmt_store::{Message as Row, Role};
use regex::Regex;
use serde_json::Value;

use super::summary::{Sections, calls};
use super::{is_real_user, is_summary};

/// File tools whose `path` argument names a written file.
const WRITERS: [&str; 2] = ["write_file", "patch"];

static PATCH_FILE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^\*\*\* (?:Add|Update|Delete) File: (.+)$").expect("valid pattern")
});

/// The items of a region.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Items {
    /// Files written or patched, in first-touch order.
    pub files: Vec<String>,
    /// The latest real user request.
    pub latest_request: Option<String>,
    /// `<tool>: <error>` of failures not followed by a success of the same
    /// tool.
    pub errors: Vec<String>,
}

impl Items {
    /// Extracts the items of `rows` (the region, unpruned).
    #[must_use]
    pub fn of(rows: &[Row]) -> Self {
        let mut items = Self {
            latest_request: rows
                .iter()
                .rev()
                .find(|r| is_real_user(r))
                .and_then(|r| r.content.clone()),
            ..Self::default()
        };
        let mut seen = HashSet::new();
        for (name, arguments) in rows
            .iter()
            .filter(|r| r.role == Role::Assistant)
            .flat_map(calls)
        {
            for path in written(&name, &arguments) {
                if seen.insert(path.clone()) {
                    items.files.push(path);
                }
            }
        }
        items.errors = unresolved_errors(rows);
        items
    }
}

/// The files a `write_file` / `patch` call writes.
fn written(name: &str, arguments: &str) -> Vec<String> {
    if !WRITERS.contains(&name) {
        return Vec::new();
    }
    let Ok(args) = serde_json::from_str::<Value>(arguments) else {
        return Vec::new();
    };
    if let Some(path) = args.get("path").and_then(Value::as_str) {
        return vec![path.to_owned()];
    }
    let patch = args
        .get("patch")
        .and_then(Value::as_str)
        .unwrap_or_default();
    PATCH_FILE
        .captures_iter(patch)
        .map(|c| c[1].trim().to_owned())
        .collect()
}

fn is_error(row: &Row) -> bool {
    row.metadata
        .as_ref()
        .and_then(|m| m.get("is_error"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Tool errors with no later success of the same tool.
fn unresolved_errors(rows: &[Row]) -> Vec<String> {
    let results: Vec<&Row> = rows.iter().filter(|r| r.role == Role::Tool).collect();
    results
        .iter()
        .enumerate()
        .filter(|(_, r)| is_error(r))
        .filter(|(i, r)| {
            !results[i + 1..]
                .iter()
                .any(|later| later.tool_name == r.tool_name && !is_error(later))
        })
        .map(|(_, r)| {
            let text: String = r
                .content
                .as_deref()
                .unwrap_or_default()
                .chars()
                .take(300)
                .collect();
            format!("{}: {text}", r.tool_name.as_deref().unwrap_or("tool"))
        })
        .collect()
}

/// The deterministic summary: the previous summary's sections plus the
/// region's items and prune lines. Like any summary it becomes the
/// previous summary of the next compaction.
#[must_use]
pub fn fallback(previous: Option<&str>, prune_lines: &[String], items: &Items) -> Sections {
    let mut sections = previous.map(Sections::parse).unwrap_or_default();
    if sections.get("Goal").is_none()
        && let Some(request) = &items.latest_request
    {
        sections.append("Goal", request);
    }
    sections.append(
        "Progress",
        "(automatic summary unavailable: deterministic record of the compacted messages)",
    );
    if let Some(request) = &items.latest_request {
        sections.append("Progress", &format!("Latest user request: {request}"));
    }
    sections.append("Progress", &bullets(prune_lines));
    sections.append("Progress", &bullets(&items.errors));
    sections.append("Files", &bullets(&items.files));
    sections
}

fn bullets(lines: &[String]) -> String {
    lines
        .iter()
        .map(|l| format!("- {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The previous summary body among `rows`, if any.
#[must_use]
pub fn previous_summary(rows: &[Row]) -> Option<String> {
    rows.iter()
        .rev()
        .find(|r| is_summary(r))
        .and_then(|r| r.content.as_deref())
        .map(super::summary::body_of)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::compress::fixtures::{Build as _, row};

    #[test]
    fn items_of_a_region() {
        let patch = json!({"patch": "*** Begin Patch\n*** Update File: src/auth.rs\n@@\n-a\n+b\n*** End Patch"});
        let rows = vec![
            row(Role::User, "fix the login"),
            row(Role::Assistant, "")
                .calling("c1", "write_file", r#"{"path":"src/a.rs","content":"x"}"#)
                .calling("c2", "patch", &patch.to_string())
                .calling("c3", "terminal", r#"{"command":"cargo test"}"#),
            row(Role::Tool, "ok").answering_as("c1", "write_file"),
            row(Role::Tool, "ok").answering_as("c2", "patch"),
            row(Role::Tool, "boom")
                .answering_as("c3", "terminal")
                .meta("is_error", json!(true)),
            row(Role::Assistant, "").calling("c4", "read_file", "{}"),
            row(Role::Tool, "nope")
                .answering_as("c4", "read_file")
                .meta("is_error", json!(true)),
            row(Role::Assistant, "").calling("c5", "read_file", "{}"),
            row(Role::Tool, "fine").answering_as("c5", "read_file"),
            row(Role::User, "notice").meta("prometheus_authored", json!(true)),
        ];
        let items = Items::of(&rows);
        assert_eq!(items.files, ["src/a.rs", "src/auth.rs"]);
        assert_eq!(items.latest_request.as_deref(), Some("fix the login"));
        assert_eq!(items.errors, ["terminal: boom"]);
    }

    #[test]
    fn the_fallback_keeps_the_previous_summary_and_adds_the_record() {
        let items = Items {
            files: vec!["src/auth.rs".into()],
            latest_request: Some("fix the login".into()),
            errors: Vec::new(),
        };
        let lines = vec!["[pruned tool result] terminal({}) -> 900 chars, ref 0123456789ab".into()];
        let doc = fallback(
            Some("## Goal\nS1 goal\n## Decisions\nUse JWT."),
            &lines,
            &items,
        )
        .document(None);
        assert!(doc.contains("## Goal\nS1 goal"));
        assert!(doc.contains("## Decisions\nUse JWT."));
        assert!(doc.contains("- [pruned tool result] terminal({}) -> 900 chars, ref 0123456789ab"));
        assert!(doc.contains("## Files\n- src/auth.rs"));
        assert!(doc.contains("Latest user request: fix the login"));
    }
}

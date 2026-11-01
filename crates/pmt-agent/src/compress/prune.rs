//! The deterministic first phase of a compaction (spec:
//! context-compression, Deterministic recoverable prune).
//!
//! It runs without any model call on a copy of the rows being summarized:
//! a tool result repeated verbatim later becomes a duplicate marker, a
//! tool result over 200 characters becomes one line naming the tool, its
//! arguments, its size and a short ref, and an inline image payload
//! becomes an `[image removed, ref ...]` marker. Every replaced original
//! is returned so the commit keeps it in the blob store under that ref
//! (the first 12 hex characters of its SHA-256). Tool-call arguments are
//! never touched, and nothing from the current turn (its user message on:
//! the unanswered tool round, skill bodies loaded this turn) is pruned.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::LazyLock;

use pmt_store::{BlobRef, Message as Row, Role};
use regex::Regex;
use serde_json::Value;

use super::is_real_user;

/// What an earlier copy of a repeated tool result becomes.
pub const DUPLICATE: &str = "[duplicate tool output, same as a later call]";
/// Tool results longer than this many characters are pruned.
pub const MIN_PRUNE_CHARS: usize = 200;
/// The longest arguments digest of a prune line.
const DIGEST_MAX: usize = 120;
/// Hex characters of a short ref.
pub const REF_LEN: usize = 12;

static INLINE_IMAGE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"data:image/[A-Za-z0-9.+-]+;base64,[A-Za-z0-9+/=_-]+").expect("valid pattern")
});

/// The pruned copy of a region.
#[derive(Debug, Clone, Default)]
pub struct Pruned {
    /// The region's rows, pruned.
    pub rows: Vec<Row>,
    /// Every prune line written, in order.
    pub lines: Vec<String>,
    /// The originals the refs name (keep each in the blob store).
    pub originals: Vec<Vec<u8>>,
}

/// The short ref of `bytes`.
#[must_use]
pub fn short_ref(bytes: &[u8]) -> String {
    BlobRef::of(bytes).sha256()[..REF_LEN].to_owned()
}

/// Prunes the rows of `region` (indices into `rows`, the whole active
/// transcript; later rows are read for duplicates).
#[must_use]
pub fn prune(rows: &[Row], region: Range<usize>) -> Pruned {
    let current_turn = rows.iter().rposition(is_real_user).unwrap_or(rows.len());
    let calls = calls_by_id(rows);
    let mut out = Pruned::default();
    for i in region {
        let mut row = rows[i].clone();
        if i < current_turn {
            prune_row(&mut row, &rows[i + 1..], &calls, &mut out);
        }
        out.rows.push(row);
    }
    out
}

fn prune_row(row: &mut Row, later: &[Row], calls: &HashMap<String, Call>, out: &mut Pruned) {
    let Some(text) = row.content.clone() else {
        return;
    };
    if row.role == Role::Tool && text.len() > DUPLICATE.len() && repeated(&text, later) {
        row.content = Some(DUPLICATE.to_owned());
        return;
    }
    let text = strip_images(&text, out);
    if row.role == Role::Tool && text.chars().count() > MIN_PRUNE_CHARS {
        let line = prune_line(row, &text, calls);
        out.originals.push(text.into_bytes());
        out.lines.push(line.clone());
        row.content = Some(line);
    } else {
        row.content = Some(text);
    }
}

/// Whether a later tool result has exactly `text`.
fn repeated(text: &str, later: &[Row]) -> bool {
    later
        .iter()
        .any(|r| r.role == Role::Tool && r.content.as_deref() == Some(text))
}

/// `text` with each inline image payload replaced by a marker.
fn strip_images(text: &str, out: &mut Pruned) -> String {
    INLINE_IMAGE
        .replace_all(text, |found: &regex::Captures<'_>| {
            let bytes = found[0].as_bytes().to_vec();
            let marker = format!("[image removed, ref {}]", short_ref(&bytes));
            out.originals.push(bytes);
            marker
        })
        .into_owned()
}

/// `[pruned tool result] <tool>(<digest>) -> <N> chars, ref <ref>`.
fn prune_line(row: &Row, text: &str, calls: &HashMap<String, Call>) -> String {
    let call = row.tool_call_id.as_ref().and_then(|id| calls.get(id));
    let tool = row
        .tool_name
        .clone()
        .or_else(|| call.map(|c| c.name.clone()))
        .unwrap_or_else(|| "tool".to_owned());
    let digest = call.map(|c| digest(&c.arguments)).unwrap_or_default();
    format!(
        "[pruned tool result] {tool}({digest}) -> {} chars, ref {}",
        text.chars().count(),
        short_ref(text.as_bytes())
    )
}

/// The arguments on one line, at most 120 characters.
fn digest(arguments: &str) -> String {
    let flat = arguments.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= DIGEST_MAX {
        return flat;
    }
    let mut cut: String = flat.chars().take(DIGEST_MAX - 3).collect();
    cut.push_str("...");
    cut
}

struct Call {
    name: String,
    arguments: String,
}

fn calls_by_id(rows: &[Row]) -> HashMap<String, Call> {
    let mut calls = HashMap::new();
    for call in rows
        .iter()
        .filter_map(|r| r.tool_calls.as_ref())
        .filter_map(Value::as_array)
        .flatten()
    {
        let field = |key: &str| call.get(key).and_then(Value::as_str).unwrap_or_default();
        calls.insert(
            field("id").to_owned(),
            Call {
                name: field("name").to_owned(),
                arguments: field("arguments").to_owned(),
            },
        );
    }
    calls
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compress::fixtures::{Build as _, row};

    fn contents(pruned: &Pruned) -> Vec<String> {
        pruned
            .rows
            .iter()
            .map(|r| r.content.clone().unwrap_or_default())
            .collect()
    }

    /// Golden transcript: a large old terminal result, a repeated read, a
    /// short result, an inline image and the current turn.
    #[test]
    fn golden_prune() {
        let big = "line of output\n".repeat(2_667); // 40,005 chars
        let big = &big[..40_000];
        let image = "see data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAE= here";
        let rows = vec![
            row(Role::User, "build it"),
            row(Role::Assistant, "").calling("c1", "terminal", r#"{"command": "make  all"}"#),
            row(Role::Tool, big).answering_as("c1", "terminal"),
            row(Role::Assistant, "").calling("c2", "read_file", r#"{"path":"a.rs"}"#),
            row(Role::Tool, &"fn a() {}\n".repeat(30)).answering_as("c2", "read_file"),
            row(Role::Assistant, "").calling("c3", "lookup", r#"{"q":"x"}"#),
            row(Role::Tool, "short").answering("c3"),
            row(Role::User, image),
            row(Role::Assistant, "").calling("c4", "read_file", r#"{"path":"a.rs"}"#),
            row(Role::Tool, &"fn a() {}\n".repeat(30)).answering_as("c4", "read_file"),
            row(Role::User, "now the current turn"),
            row(Role::Assistant, "").calling("c5", "terminal", "{}"),
            row(Role::Tool, &"current ".repeat(100)).answering_as("c5", "terminal"),
        ];
        let pruned = prune(&rows, 0..rows.len());
        let ref_big = short_ref(big.as_bytes());
        let ref_image = short_ref(b"data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAE=");
        let ref_read = short_ref("fn a() {}\n".repeat(30).as_bytes());
        assert_eq!(
            contents(&pruned),
            [
                "build it".to_owned(),
                String::new(),
                format!(
                    "[pruned tool result] terminal({{\"command\": \"make all\"}}) -> 40000 chars, ref {ref_big}"
                ),
                String::new(),
                DUPLICATE.to_owned(),
                String::new(),
                "short".to_owned(),
                format!("see [image removed, ref {ref_image}] here"),
                String::new(),
                format!(
                    "[pruned tool result] read_file({{\"path\":\"a.rs\"}}) -> 300 chars, ref {ref_read}"
                ),
                "now the current turn".to_owned(),
                String::new(),
                "current ".repeat(100),
            ]
        );
        // Arguments are never pruned.
        assert_eq!(pruned.rows[1].tool_calls, rows[1].tool_calls);
        // Each ref names its original.
        assert_eq!(pruned.originals.len(), 3);
        assert_eq!(short_ref(&pruned.originals[0]), ref_big);
        assert_eq!(pruned.originals[0].len(), 40_000);
        assert_eq!(pruned.lines.len(), 2);
    }

    #[test]
    fn a_long_argument_digest_is_cut_at_120_characters() {
        let arguments = format!(r#"{{"command":"{}"}}"#, "y".repeat(300));
        let rows = vec![
            row(Role::Assistant, "").calling("c1", "terminal", &arguments),
            row(Role::Tool, &"z".repeat(500)).answering_as("c1", "terminal"),
            row(Role::User, "next"),
        ];
        let pruned = prune(&rows, 0..2);
        let line = pruned.rows[1].content.clone().unwrap();
        let digest = line
            .strip_prefix("[pruned tool result] terminal(")
            .and_then(|rest| rest.split(") -> ").next())
            .unwrap();
        assert_eq!(digest.chars().count(), 120);
        assert!(digest.ends_with("..."));
    }
}

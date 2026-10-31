//! `read_file` (spec: execution-environments, `read_file` guards, pagination
//! and dedup; `read_file` encoding, conflict markers and path suggestions;
//! Protected secret files and device-namespace paths).

use std::io;
use std::path::Path;

use serde_json::{Map, Value, json};

use super::backend::Kind;
use super::text::{self, Encoding};
use super::tracker::{self, ReadKey};
use super::{FileSession, guards, io_failure, json_result, lines};
use crate::result::{Status, ToolResult};

/// Lines returned when the model gives no `limit`.
pub const DEFAULT_LIMIT: usize = 2000;

/// Suggestions offered for a missing path.
const MAX_SUGGESTIONS: usize = 5;

/// Names at least this similar to the missing one are suggested.
const SUGGESTION_SIMILARITY: f64 = 0.5;

/// `read_file` arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadArgs {
    /// The path as the model wrote it.
    pub path: String,
    /// First line (1-based).
    pub offset: usize,
    /// Most lines returned.
    pub limit: usize,
}

impl ReadArgs {
    /// Reads `path` from line 1, [`DEFAULT_LIMIT`] lines.
    #[must_use]
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            offset: 1,
            limit: DEFAULT_LIMIT,
        }
    }
}

/// One page of numbered lines.
struct Page {
    content: String,
    first: usize,
    last: usize,
    next_offset: Option<usize>,
}

impl FileSession {
    /// Reads a page of a text file.
    #[must_use]
    pub fn read_file(&self, args: &ReadArgs) -> ToolResult {
        let shown = args.path.as_str();
        if let Some(refusal) = guards::device_namespace(shown) {
            return ToolResult::text(Status::Denied, refusal);
        }
        let path = self.resolve(shown);
        if let Some(refusal) = guards::read_refusal(&path, shown) {
            return ToolResult::text(Status::Denied, refusal);
        }
        let bytes = match self.read_bytes(&path, shown) {
            Ok(bytes) => bytes,
            Err(result) => return *result,
        };
        let Some((decoded, encoding)) = text::decode(&bytes) else {
            return ToolResult::error(format!(
                "Cannot read binary file '{shown}'; use vision_analyze for images or terminal for binary inspection"
            ));
        };
        self.page_result(args, &path, &bytes, &decoded, encoding)
    }

    /// The file's bytes, after the kind checks; `Err` is the final result.
    fn read_bytes(&self, path: &Path, shown: &str) -> Result<Vec<u8>, Box<ToolResult>> {
        let meta = match self.backend().metadata(path) {
            Ok(meta) => meta,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Err(Box::new(ToolResult::error(self.not_found(path, shown))));
            }
            Err(err) => return Err(Box::new(io_failure(shown, &err))),
        };
        let refusal = match meta.kind {
            Kind::File => None,
            Kind::Dir => Some(format!("'{shown}' is a directory, not a file")),
            Kind::CharDevice | Kind::BlockDevice => Some(format!(
                "Cannot read '{shown}': this is a device file that would block or produce infinite output."
            )),
            Kind::Fifo => Some(format!(
                "Cannot read '{shown}': this is a named pipe that would block."
            )),
            Kind::Socket | Kind::Other => Some(format!(
                "Cannot read '{shown}': this is not a regular file."
            )),
        };
        if let Some(refusal) = refusal {
            return Err(Box::new(ToolResult::error(refusal)));
        }
        self.backend()
            .read(path)
            .map_err(|err| Box::new(io_failure(shown, &err)))
    }

    fn page_result(
        &self,
        args: &ReadArgs,
        path: &Path,
        bytes: &[u8],
        decoded: &str,
        encoding: Encoding,
    ) -> ToolResult {
        let all = lines(decoded);
        let shown = args.path.as_str();
        if args.offset > all.len().max(1) {
            return ToolResult::error(format!(
                "offset {} is past the end of '{shown}' ({} lines)",
                args.offset,
                all.len()
            ));
        }
        let hash = tracker::hash(bytes);
        let page = paginate(&all, args.offset, args.limit, self.setup.read_max_chars);
        let key = ReadKey {
            path: path.to_path_buf(),
            offset: args.offset,
            limit: args.limit,
            hash,
            generation: self.generation(),
        };
        let mut tracker = self.tracker();
        let before = tracker.repeat(key);
        tracker.note_read(path, hash, all.len(), page.first, page.last);
        drop(tracker);
        match before {
            0 => {}
            1 => return unchanged(shown, &page),
            _ => return ToolResult::text(Status::Blocked, blocked(shown, &page)),
        }
        let returned = all
            .get(page.first.saturating_sub(1)..page.last)
            .unwrap_or_default();
        render(
            shown,
            &page,
            all.len(),
            encoding,
            text::conflict_blocks(returned.iter().copied()),
        )
    }

    /// `File not found: <path>` plus up to five similar existing paths of
    /// the same or the parent directory.
    fn not_found(&self, path: &Path, shown: &str) -> String {
        let mut message = format!("File not found: {shown}");
        let suggestions = self.suggestions(path, shown);
        if !suggestions.is_empty() {
            message.push_str("\nDid you mean:");
            for suggestion in suggestions {
                message.push_str("\n  ");
                message.push_str(&suggestion);
            }
        }
        message
    }

    fn suggestions(&self, path: &Path, shown: &str) -> Vec<String> {
        let Some(wanted) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            return Vec::new();
        };
        let shown_path = Path::new(shown);
        let shown_parent = shown_path.parent().unwrap_or(Path::new(""));
        let mut scored: Vec<(f64, String)> = Vec::new();
        let dirs = [
            (
                path.parent().map(Path::to_path_buf),
                shown_parent.to_path_buf(),
            ),
            (
                path.parent().and_then(Path::parent).map(Path::to_path_buf),
                shown_parent.parent().unwrap_or(Path::new("")).to_path_buf(),
            ),
        ];
        for (dir, shown_dir) in dirs {
            let Some(dir) = dir else { continue };
            scored.extend(self.similar_entries(&dir, &shown_dir, &wanted));
        }
        scored.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        scored.dedup_by(|a, b| a.1 == b.1);
        scored
            .into_iter()
            .take(MAX_SUGGESTIONS)
            .map(|(_, p)| p)
            .collect()
    }

    fn similar_entries(&self, dir: &Path, shown_dir: &Path, wanted: &str) -> Vec<(f64, String)> {
        let Ok(entries) = self.backend().list_dir(dir) else {
            return Vec::new();
        };
        entries
            .into_iter()
            .filter(|entry| matches!(entry.kind, Kind::File | Kind::Dir))
            .filter_map(|entry| {
                let score = text::similarity(wanted, &entry.name);
                (score >= SUGGESTION_SIMILARITY).then(|| (score, joined(shown_dir, &entry.name)))
            })
            .collect()
    }
}

/// `dir/name` with the separator the model used (`/` unless it wrote
/// backslashes), so a suggestion reads like the path it corrects.
fn joined(dir: &Path, name: &str) -> String {
    let dir = dir.to_string_lossy();
    if dir.is_empty() {
        return name.to_owned();
    }
    let separator = if dir.contains('\\') { '\\' } else { '/' };
    format!("{}{separator}{name}", dir.trim_end_matches(['/', '\\']))
}

/// Numbered lines from `offset`, at most `limit` of them and at most
/// `max_chars` formatted characters (one over-long line is cut).
fn paginate(all: &[&str], offset: usize, limit: usize, max_chars: usize) -> Page {
    let first = offset.max(1);
    let mut content = String::new();
    let mut last = first.saturating_sub(1);
    for (index, line) in all.iter().enumerate().skip(first - 1).take(limit.max(1)) {
        let number = index + 1;
        let formatted = format!("{number:>6}|{line}\n");
        let chars = content.chars().count() + formatted.chars().count();
        if chars > max_chars {
            if content.is_empty() {
                let keep: String = formatted.chars().take(max_chars).collect();
                content = format!("{keep}... [line truncated]\n");
                last = number;
            }
            break;
        }
        content.push_str(&formatted);
        last = number;
    }
    Page {
        content,
        first,
        last,
        next_offset: (last < all.len()).then_some(last + 1),
    }
}

fn render(
    shown: &str,
    page: &Page,
    total: usize,
    encoding: Encoding,
    conflicts: usize,
) -> ToolResult {
    let mut body = Map::new();
    body.insert("path".into(), json!(shown));
    body.insert("content".into(), json!(page.content));
    body.insert("total_lines".into(), json!(total));
    body.insert(
        "lines".into(),
        json!(format!("{}-{}", page.first, page.last)),
    );
    if let Some(next) = page.next_offset {
        body.insert("next_offset".into(), json!(next));
    }
    if encoding != Encoding::Utf8 {
        body.insert("encoding".into(), json!(encoding.label()));
    }
    let mut hints = Vec::new();
    if conflicts > 0 {
        body.insert("conflict_blocks".into(), json!(conflicts));
        hints.push("resolve the merge conflicts before editing around them".to_owned());
    }
    if let Some(next) = page.next_offset {
        hints.push(format!("more lines remain; read on with offset={next}"));
    }
    if !hints.is_empty() {
        body.insert("hint".into(), json!(hints.join("; ")));
    }
    json_result(Status::Ok, &Value::Object(body))
}

fn unchanged(shown: &str, page: &Page) -> ToolResult {
    json_result(
        Status::Ok,
        &json!({
            "status": "unchanged",
            "path": shown,
            "lines": format!("{}-{}", page.first, page.last),
            "message": "this region has not changed since your last read; use that earlier result",
        }),
    )
}

fn blocked(shown: &str, page: &Page) -> String {
    format!(
        "BLOCKED: You have already read this exact region of '{shown}' (lines {}-{}) and it has not changed. Use the earlier result instead of reading it again.",
        page.first, page.last
    )
}

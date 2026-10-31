//! `search_files` (spec: execution-environments, `search_files` contract).
//!
//! On a host-filesystem backend the search runs in-process: a
//! `.gitignore`-aware walk that skips VCS directories and binary files,
//! the built-in regex engine for content and globs for names. Backends in
//! their own namespace (M2) run `rg` (falling back to `grep -rn`) inside
//! the environment instead.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;
use pmt_core::capability::Access;
use regex::Regex;
use serde_json::{Map, Value, json};

use super::{FileSession, guards, io_failure, json_result, shown, text};
use crate::result::{Status, ToolResult};

/// Entries returned when the model gives no `limit`.
pub const DEFAULT_LIMIT: usize = 50;
/// The largest `limit`.
pub const MAX_LIMIT: usize = 500;
/// The largest `context`.
pub const MAX_CONTEXT: usize = 10;
/// Characters kept of a matching line.
pub const MAX_LINE_CHARS: usize = 500;

/// Directories never searched.
const VCS_DIRS: &[&str] = &[".git", ".hg", ".svn", ".jj", ".bzr"];

/// What to match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// Lines of file content (regex).
    Content,
    /// File names (glob).
    Files,
}

/// Result ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    /// Walk order (sorted by name within a directory).
    Discovery,
    /// Newest modification first.
    Modified,
}

/// What a content search returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    /// `<path>:<line>:<text>` per matching line.
    Content,
    /// Paths of matching files.
    FilesOnly,
    /// Match counts per file.
    Count,
}

/// `search_files` arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchArgs {
    /// Regex (content) or glob (files).
    pub pattern: String,
    /// What to match.
    pub target: Target,
    /// Where to search; `None` is the session cwd.
    pub path: Option<String>,
    /// Only files whose names match this glob.
    pub file_glob: Option<String>,
    /// Entries per page (1..=500).
    pub limit: usize,
    /// Entries skipped.
    pub offset: usize,
    /// Ordering.
    pub order: Order,
    /// Output shape of a content search.
    pub output_mode: OutputMode,
    /// Lines of context around each match (0..=10).
    pub context: usize,
}

impl SearchArgs {
    /// A content search for `pattern` with the defaults.
    #[must_use]
    pub fn new(pattern: impl Into<String>) -> Self {
        Self {
            pattern: pattern.into(),
            target: Target::Content,
            path: None,
            file_glob: None,
            limit: DEFAULT_LIMIT,
            offset: 0,
            order: Order::Discovery,
            output_mode: OutputMode::Content,
            context: 0,
        }
    }

    /// Reads the tool arguments (the schema already checked types and
    /// enums).
    #[must_use]
    pub fn from_value(args: &Value) -> Self {
        let text = |key: &str| args.get(key).and_then(Value::as_str);
        let number = |key: &str| {
            args.get(key)
                .and_then(Value::as_u64)
                .and_then(|n| usize::try_from(n).ok())
        };
        Self {
            pattern: text("pattern").unwrap_or_default().to_owned(),
            target: match text("target") {
                Some("files") => Target::Files,
                _ => Target::Content,
            },
            path: text("path").map(str::to_owned),
            file_glob: text("file_glob").map(str::to_owned),
            limit: number("limit").unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT),
            offset: number("offset").unwrap_or(0),
            order: match text("order") {
                Some("modified") => Order::Modified,
                _ => Order::Discovery,
            },
            output_mode: match text("output_mode") {
                Some("files_only") => OutputMode::FilesOnly,
                Some("count") => OutputMode::Count,
                _ => OutputMode::Content,
            },
            context: number("context").unwrap_or(0).min(MAX_CONTEXT),
        }
    }
}

/// One walked file.
struct Walked {
    path: PathBuf,
    modified: Option<SystemTime>,
}

impl FileSession {
    /// Searches file content or names.
    #[must_use]
    pub fn search_files(&self, args: &SearchArgs) -> ToolResult {
        if !self.backend().uses_host_filesystem() {
            return ToolResult::error(format!(
                "search_files on the '{}' backend is not available in this build",
                self.backend().name()
            ));
        }
        if let Some(refusal) = args.path.as_deref().and_then(guards::device_namespace) {
            return ToolResult::text(Status::Denied, refusal);
        }
        let cwd = self.cwd();
        let root = args
            .path
            .as_deref()
            .map_or_else(|| cwd.clone(), |p| self.resolve(p));
        let shown_root = args.path.clone().unwrap_or_else(|| ".".to_owned());
        if let Err(err) = self.backend().check(Access::Read, &root) {
            return io_failure(&shown_root, &err);
        }
        if !root.exists() {
            return ToolResult::error(format!("File not found: {shown_root}"));
        }
        // The pattern is checked before the (possibly long) walk.
        let matcher = match Matcher::compile(args) {
            Ok(matcher) => matcher,
            Err(why) => return ToolResult::error(why),
        };
        let files = match walk(&root, args.file_glob.as_deref()) {
            Ok(files) => files,
            Err(why) => return ToolResult::error(why),
        };
        let files = ordered(files, args.order);
        match matcher {
            Matcher::Name(pattern) => by_name(&files, &pattern, args, &cwd),
            Matcher::Content(regex) => by_content(&files, &regex, args, &cwd),
        }
    }
}

/// The compiled `pattern`: a glob for names or a regex for content.
enum Matcher {
    Name(glob::Pattern),
    Content(Regex),
}

impl Matcher {
    fn compile(args: &SearchArgs) -> Result<Self, String> {
        let invalid = |err: &dyn std::fmt::Display| format!("invalid pattern: {err}");
        match args.target {
            Target::Files => glob::Pattern::new(&args.pattern)
                .map(Self::Name)
                .map_err(|err| invalid(&err)),
            Target::Content => Regex::new(&args.pattern)
                .map(Self::Content)
                .map_err(|err| invalid(&err)),
        }
    }
}

/// Whether reading `path` is refused (protected secret files such as a
/// project `.env`, also behind a link); a content search must not leak
/// what `read_file` refuses. Name searches still list such files: a name
/// is not a secret.
fn protected(path: &Path) -> bool {
    guards::read_refusal(path, &path.to_string_lossy()).is_some()
}

/// Every regular file below `root`, honouring `.gitignore` (also outside
/// a git repository) and skipping VCS directories.
fn walk(root: &Path, file_glob: Option<&str>) -> Result<Vec<Walked>, String> {
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(false)
        .require_git(false)
        .follow_links(false)
        .sort_by_file_name(std::cmp::Ord::cmp)
        .filter_entry(|entry| {
            !(entry.file_type().is_some_and(|t| t.is_dir())
                && VCS_DIRS.contains(&entry.file_name().to_string_lossy().as_ref()))
        });
    if let Some(glob) = file_glob {
        let mut overrides = OverrideBuilder::new(root);
        overrides
            .add(glob)
            .map_err(|err| format!("invalid file_glob: {err}"))?;
        builder.overrides(
            overrides
                .build()
                .map_err(|err| format!("invalid file_glob: {err}"))?,
        );
    }
    Ok(builder
        .build()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_some_and(|t| t.is_file()))
        .map(|entry| Walked {
            modified: entry.metadata().ok().and_then(|m| m.modified().ok()),
            path: entry.into_path(),
        })
        .collect())
}

fn ordered(mut files: Vec<Walked>, order: Order) -> Vec<Walked> {
    if order == Order::Modified {
        files.sort_by_key(|file| std::cmp::Reverse(file.modified));
    }
    files
}

fn by_name(files: &[Walked], pattern: &glob::Pattern, args: &SearchArgs, cwd: &Path) -> ToolResult {
    let names: Vec<String> = files
        .iter()
        .filter(|file| {
            let name = file
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned());
            name.is_some_and(|n| pattern.matches(&n))
                || pattern.matches_path(Path::new(&shown(&file.path, cwd)))
        })
        .map(|file| shown(&file.path, cwd))
        .collect();
    let total = names.len();
    page_result("files", names, total, args)
}

fn by_content(files: &[Walked], regex: &Regex, args: &SearchArgs, cwd: &Path) -> ToolResult {
    let mut entries: Vec<Value> = Vec::new();
    let mut total = 0;
    for file in files.iter().filter(|file| !protected(&file.path)) {
        let Ok(bytes) = std::fs::read(&file.path) else {
            continue;
        };
        let Some((content, _)) = text::decode(&bytes) else {
            continue;
        };
        let all = super::lines(&content);
        let hits: Vec<usize> = (0..all.len()).filter(|&i| regex.is_match(all[i])).collect();
        if hits.is_empty() {
            continue;
        }
        let path = shown(&file.path, cwd);
        total += hits.len();
        match args.output_mode {
            OutputMode::FilesOnly => entries.push(json!(path)),
            OutputMode::Count => entries.push(json!({"path": path, "count": hits.len()})),
            OutputMode::Content => {
                entries.extend(
                    content_lines(&path, &all, &hits, args.context)
                        .into_iter()
                        .map(Value::String),
                );
            }
        }
    }
    let (key, count) = match args.output_mode {
        OutputMode::Content => ("matches", total),
        OutputMode::FilesOnly => ("files", entries.len()),
        // `total_matches` counts matching lines in count mode too.
        OutputMode::Count => ("counts", total),
    };
    page_result(key, entries, count, args)
}

/// `<path>:<line>:<text>` for each hit and `<path>-<line>-<text>` for its
/// context lines; separate groups split by `--`.
fn content_lines(path: &str, all: &[&str], hits: &[usize], context: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut shown_until: Option<usize> = None;
    for &hit in hits {
        let from = hit.saturating_sub(context);
        let to = (hit + context).min(all.len() - 1);
        let start = shown_until.map_or(from, |done| from.max(done + 1));
        if context > 0 && shown_until.is_some_and(|done| from > done + 1) {
            out.push("--".to_owned());
        }
        for (index, line) in all.iter().enumerate().take(to + 1).skip(start) {
            let separator = if hits.binary_search(&index).is_ok() {
                ':'
            } else {
                '-'
            };
            let text: String = line.chars().take(MAX_LINE_CHARS).collect();
            out.push(format!("{path}{separator}{}{separator}{text}", index + 1));
        }
        shown_until = Some(shown_until.map_or(to, |done| done.max(to)));
    }
    out
}

/// One page of `entries` under `key`, with `total_matches`, `truncated`
/// and the next `offset` when more remain.
fn page_result<T: Into<Value>>(
    key: &str,
    entries: Vec<T>,
    total: usize,
    args: &SearchArgs,
) -> ToolResult {
    let available = entries.len();
    let page: Vec<Value> = entries
        .into_iter()
        .skip(args.offset)
        .take(args.limit)
        .map(Into::into)
        .collect();
    let end = args.offset + page.len();
    let mut body = Map::new();
    body.insert(key.to_owned(), Value::Array(page));
    body.insert("total_matches".into(), json!(total));
    body.insert("truncated".into(), json!(end < available));
    if end < available {
        body.insert("next_offset".into(), json!(end));
    }
    json_result(Status::Ok, &Value::Object(body))
}

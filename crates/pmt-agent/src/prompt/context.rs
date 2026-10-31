//! Project context discovery (spec: agent-turn-loop, Project context
//! discovery; Context file scanning and size cap; Context file read
//! timeout).
//!
//! Exactly one kind of project context is loaded, first match wins:
//! `PROMETHEUS.md`/`.prometheus.md` nearest to cwd, the `AGENTS.md` chain
//! from the git root down to cwd, `CLAUDE.md` in cwd, or Cursor rules in
//! cwd. Every file is read under a timeout (a hung network mount must not
//! stall session start), threat-scanned and capped.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use super::scan;

/// The block heading.
pub const HEADING: &str = "# Project Context";

/// Characters a file keeps when the window is unknown, and the bounds of
/// the automatic cap.
pub const MIN_CAP: usize = 20_000;
/// Upper bound of the automatic cap.
pub const MAX_CAP: usize = 500_000;

/// The loaded block and what went wrong on the way.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Loaded {
    /// `# Project Context` and every file, or `None` when nothing loaded.
    pub block: Option<String>,
    /// The files that were loaded, as shown in the block.
    pub files: Vec<String>,
    /// Skipped files (timeouts, read errors), already logged at WARN.
    pub warnings: Vec<String>,
}

/// How files are read.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Characters kept per file.
    pub cap: usize,
    /// Time allowed per file read.
    pub read_timeout: Duration,
}

/// The per-file character cap: `configured` when non-zero, else 6% of the
/// window's characters (4 per token) within [`MIN_CAP`, `MAX_CAP`].
#[must_use]
pub fn cap(configured: u64, context_window: Option<u64>) -> usize {
    if configured > 0 {
        return usize::try_from(configured).unwrap_or(usize::MAX);
    }
    let Some(window) = context_window else {
        return MIN_CAP;
    };
    let chars = window.saturating_mul(4).saturating_mul(6) / 100;
    usize::try_from(chars)
        .unwrap_or(usize::MAX)
        .clamp(MIN_CAP, MAX_CAP)
}

/// The nearest ancestor of `cwd` (itself included) holding `.git`.
#[must_use]
pub fn git_root(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors()
        .find(|dir| dir.join(".git").exists())
        .map(Path::to_path_buf)
}

/// Loads the project context of `cwd`.
#[must_use]
pub fn load(cwd: &Path, limits: Limits) -> Loaded {
    let root = git_root(cwd);
    let base = root.as_deref().unwrap_or(cwd);
    let mut reader = Reader {
        limits,
        warnings: Vec::new(),
    };
    let files = discover(cwd, root.as_deref(), &mut reader);
    let mut loaded = Loaded {
        warnings: reader.warnings,
        ..Loaded::default()
    };
    if files.is_empty() {
        return loaded;
    }
    let mut block = String::from(HEADING);
    for (path, text) in files {
        let name = display(&path, base);
        block.push_str("\n\n## ");
        block.push_str(&name);
        block.push_str("\n\n");
        block.push_str(&prepare(&name, &text, limits.cap));
        loaded.files.push(name);
    }
    loaded.block = Some(block);
    loaded
}

/// The scanned, capped body of one file.
#[must_use]
pub fn prepare(name: &str, text: &str, cap: usize) -> String {
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);
    let found = scan::findings(text);
    if !found.is_empty() {
        tracing::warn!(file = name, findings = ?found, "context file blocked");
        return scan::blocked_marker(name, &found);
    }
    truncate(name, text.trim_end(), cap)
}

/// Keeps 70% of `cap` from the head and 20% from the tail of an oversized
/// text, with a marker naming what was cut.
#[must_use]
pub fn truncate(name: &str, text: &str, cap: usize) -> String {
    let total = text.chars().count();
    if total <= cap {
        return text.to_owned();
    }
    let head = cap * 7 / 10;
    let tail = cap * 2 / 10;
    let cut = total - head - tail;
    let head_end = text.char_indices().nth(head).map_or(text.len(), |(i, _)| i);
    let tail_start = text
        .char_indices()
        .nth(total - tail)
        .map_or(text.len(), |(i, _)| i);
    format!(
        "{}\n\n[... {cut} characters of {name} truncated; read the file for the full text ...]\n\n{}",
        &text[..head_end],
        &text[tail_start..]
    )
}

struct Reader {
    limits: Limits,
    warnings: Vec<String>,
}

impl Reader {
    /// The text of `path` when it exists and reads in time; `None` (with a
    /// WARN) when the read timed out or failed.
    fn read(&mut self, path: &Path) -> Option<String> {
        let (tx, rx) = mpsc::channel();
        let owned = path.to_path_buf();
        // A plain thread: a read blocked on a hung mount can never be
        // cancelled, so it must not hold a runtime worker.
        let spawned = std::thread::Builder::new()
            .name("context-file-read".into())
            .spawn(move || {
                let _ = tx.send(std::fs::read(&owned));
            });
        if spawned.is_err() {
            return self.skip(path, "could not start the reader");
        }
        match rx.recv_timeout(self.limits.read_timeout) {
            Ok(Ok(bytes)) => Some(String::from_utf8_lossy(&bytes).into_owned()),
            Ok(Err(err)) => self.skip(path, &err.to_string()),
            Err(_) => self.skip(
                path,
                &format!(
                    "not read within {} s",
                    self.limits.read_timeout.as_secs_f32()
                ),
            ),
        }
    }

    fn skip(&mut self, path: &Path, why: &str) -> Option<String> {
        let message = format!("context file {} skipped: {why}", path.display());
        tracing::warn!("{message}");
        self.warnings.push(message);
        None
    }

    /// The first candidate in `dir` that exists and is not empty.
    fn first_non_empty(&mut self, dir: &Path, names: &[&str]) -> Option<(PathBuf, String)> {
        for name in names {
            let path = dir.join(name);
            if !exists(&path) {
                continue;
            }
            if let Some(text) = self.read(&path)
                && !text.trim().is_empty()
            {
                return Some((path, text));
            }
        }
        None
    }
}

/// An entry that is there and is not a directory (a FIFO or a file on a
/// stalled mount counts: the timed read decides).
fn exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| !m.is_dir())
}

fn discover(cwd: &Path, root: Option<&Path>, reader: &mut Reader) -> Vec<(PathBuf, String)> {
    if let Some(found) = prometheus_file(cwd, root, reader) {
        return vec![found];
    }
    let chain = agents_chain(cwd, root, reader);
    if !chain.is_empty() {
        return chain;
    }
    if let Some(found) = reader.first_non_empty(cwd, &["CLAUDE.md"]) {
        return vec![found];
    }
    cursor_rules(cwd, reader)
}

/// Directories from `root` down to `cwd` (only `cwd` without a root).
fn chain_dirs(cwd: &Path, root: Option<&Path>) -> Vec<PathBuf> {
    let Some(root) = root else {
        return vec![cwd.to_path_buf()];
    };
    let mut dirs: Vec<PathBuf> = cwd
        .ancestors()
        .take_while(|dir| dir.starts_with(root))
        .map(Path::to_path_buf)
        .collect();
    dirs.reverse();
    dirs
}

fn prometheus_file(
    cwd: &Path,
    root: Option<&Path>,
    reader: &mut Reader,
) -> Option<(PathBuf, String)> {
    let mut dirs = chain_dirs(cwd, root);
    dirs.reverse(); // nearest first
    dirs.iter().find_map(|dir| {
        reader
            .first_non_empty(dir, &["PROMETHEUS.md", ".prometheus.md"])
            .map(|(path, text)| (path, strip_frontmatter(&text).to_owned()))
    })
}

fn agents_chain(cwd: &Path, root: Option<&Path>, reader: &mut Reader) -> Vec<(PathBuf, String)> {
    let mut found: Vec<(PathBuf, String)> = Vec::new();
    for dir in chain_dirs(cwd, root) {
        let Some((path, text)) =
            reader.first_non_empty(&dir, &["AGENTS.override.md", "AGENTS.md", "agents.md"])
        else {
            continue;
        };
        if !found.iter().any(|(_, seen)| seen.trim() == text.trim()) {
            found.push((path, text));
        }
    }
    found
}

fn cursor_rules(cwd: &Path, reader: &mut Reader) -> Vec<(PathBuf, String)> {
    let mut found: Vec<(PathBuf, String)> = Vec::new();
    if let Some(rules) = reader.first_non_empty(cwd, &[".cursorrules"]) {
        found.push(rules);
    }
    let mut mdc: Vec<PathBuf> = std::fs::read_dir(cwd.join(".cursor/rules"))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "mdc"))
        .collect();
    mdc.sort();
    for path in mdc {
        if let Some(text) = reader.read(&path)
            && !text.trim().is_empty()
        {
            found.push((path, text));
        }
    }
    found
}

/// The text after a leading `---` ... `---` block.
#[must_use]
pub fn strip_frontmatter(text: &str) -> &str {
    let body = text.strip_prefix('\u{FEFF}').unwrap_or(text);
    let Some(rest) = body
        .strip_prefix("---\n")
        .or_else(|| body.strip_prefix("---\r\n"))
    else {
        return text;
    };
    let Some(end) = rest.find("\n---") else {
        return text;
    };
    let after = &rest[end + 4..];
    after
        .strip_prefix('\r')
        .unwrap_or(after)
        .trim_start_matches('\n')
}

/// `path` relative to `base` with `/` separators.
fn display(path: &Path, base: &Path) -> String {
    let relative = path.strip_prefix(base).unwrap_or(path);
    relative
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_automatic_cap_follows_the_window() {
        assert_eq!(cap(0, None), 20_000);
        assert_eq!(cap(0, Some(8_000)), 20_000);
        assert_eq!(cap(0, Some(200_000)), 48_000);
        assert_eq!(cap(0, Some(10_000_000)), 500_000);
        assert_eq!(cap(1234, Some(200_000)), 1234);
    }

    #[test]
    fn frontmatter_is_stripped() {
        assert_eq!(strip_frontmatter("---\nname: x\n---\nBody\n"), "Body\n");
        assert_eq!(strip_frontmatter("No front\n---\n"), "No front\n---\n");
    }

    #[test]
    fn truncation_keeps_head_and_tail() {
        let text: String = (0..100).map(|i| char::from(b'a' + (i % 26))).collect();
        let cut = truncate("big.md", &text, 50);
        assert!(cut.starts_with(&text[..35]), "{cut}");
        assert!(cut.ends_with(&text[90..]), "{cut}");
        assert!(
            cut.contains("[... 55 characters of big.md truncated"),
            "{cut}"
        );
    }
}

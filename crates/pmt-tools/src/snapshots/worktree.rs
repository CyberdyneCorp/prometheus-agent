//! The working-tree diff behind `/diff` (spec: execution-environments,
//! Working-tree diff command): the session directory's git repository
//! (unstaged plus untracked by default, staged, or both), or `session`,
//! the cumulative diff of every file the agent changed in this session
//! from its earliest snapshot, which needs no repository.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::journal;
use super::store::SnapshotStore;
use super::tree::Tree;

/// What `/diff` shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Unstaged changes plus untracked files.
    #[default]
    Unstaged,
    /// Staged changes.
    Staged,
    /// Both.
    All,
    /// Everything the agent changed in this session.
    Session,
}

/// A parsed `/diff` request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Request {
    /// The mode.
    pub mode: Mode,
    /// Per-file counts only.
    pub stat: bool,
    /// Limit to these paths.
    pub paths: Vec<String>,
}

impl Request {
    /// Parses `[unstaged|staged|all|session] [--stat] [<path>...]`.
    #[must_use]
    pub fn parse(args: &str) -> Self {
        let mut request = Self::default();
        for (index, word) in args.split_whitespace().enumerate() {
            match word {
                "--stat" => request.stat = true,
                "unstaged" if index == 0 => request.mode = Mode::Unstaged,
                "staged" if index == 0 => request.mode = Mode::Staged,
                "all" if index == 0 => request.mode = Mode::All,
                "session" if index == 0 => request.mode = Mode::Session,
                path => request.paths.push(path.to_owned()),
            }
        }
        request
    }
}

/// The diff `request` asks for, in `cwd`, of `session`'s changes (their
/// current content read from `tree`).
#[must_use]
pub fn diff(
    store: &SnapshotStore,
    tree: &dyn Tree,
    session: &str,
    cwd: &Path,
    request: &Request,
) -> String {
    let text = match request.mode {
        Mode::Session => session_diff(store, tree, session, cwd, request),
        mode => match git(cwd, &["rev-parse", "--show-toplevel"]) {
            Some(_) => git_diff(cwd, mode, request),
            None => return "not a git repository".to_owned(),
        },
    };
    if text.trim().is_empty() {
        "no changes".to_owned()
    } else {
        text
    }
}

fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .ok()?;
    // `diff --no-index` exits 1 when the files differ.
    (output.status.success() || output.status.code() == Some(1))
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

fn git_diff(cwd: &Path, mode: Mode, request: &Request) -> String {
    let stat: &[&str] = if request.stat { &["--stat"] } else { &[] };
    let paths: Vec<&str> = request.paths.iter().map(String::as_str).collect();
    let run = |base: &[&str]| {
        let mut args: Vec<&str> = base.to_vec();
        args.extend(stat);
        args.push("--");
        args.extend(&paths);
        git(cwd, &args).unwrap_or_default()
    };
    let mut text = String::new();
    if matches!(mode, Mode::Staged | Mode::All) {
        text.push_str(&run(&["diff", "--no-color", "--cached"]));
    }
    if matches!(mode, Mode::Unstaged | Mode::All) {
        text.push_str(&run(&["diff", "--no-color"]));
        let mut untracked_args = vec!["ls-files", "--others", "--exclude-standard", "--"];
        untracked_args.extend(&paths);
        let untracked = git(cwd, &untracked_args).unwrap_or_default();
        for file in untracked.lines().filter(|l| !l.is_empty()) {
            let mut args = vec!["diff", "--no-color", "--no-index"];
            args.extend(stat);
            args.extend(["--", null_device(), file]);
            text.push_str(&git(cwd, &args).unwrap_or_default());
        }
    }
    text
}

fn null_device() -> &'static str {
    if cfg!(windows) { "NUL" } else { "/dev/null" }
}

/// Per changed file: the content of its earliest snapshot in this
/// session and its content now.
fn session_diff(
    store: &SnapshotStore,
    tree: &dyn Tree,
    session: &str,
    cwd: &Path,
    request: &Request,
) -> String {
    let mut first: BTreeMap<PathBuf, Option<String>> = BTreeMap::new();
    let here = journal::live_edits(store, session)
        .into_iter()
        .filter(|e| e.backend.as_deref() == tree.backend());
    for edit in here {
        first.entry(edit.path).or_insert(edit.before);
    }
    let mut text = String::new();
    for (path, before) in first {
        let shown = path
            .strip_prefix(cwd)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        if !request.paths.is_empty()
            && !request
                .paths
                .iter()
                .any(|p| shown.starts_with(p.trim_start_matches("./")))
        {
            continue;
        }
        let then = before
            .and_then(|h| store.get(&h).ok())
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default();
        let now = tree
            .read(&path)
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default();
        if then == now {
            continue;
        }
        let diff = similar::TextDiff::from_lines(&then, &now);
        if request.stat {
            let _ = writeln!(text, "{shown} | {}", stat(&diff));
        } else {
            let header = diff
                .unified_diff()
                .header(&format!("a/{shown}"), &format!("b/{shown}"))
                .to_string();
            text.push_str(&header);
        }
    }
    text
}

/// `+<added> -<removed>` of a diff.
fn stat(diff: &similar::TextDiff<'_, '_, str>) -> String {
    let (mut added, mut removed) = (0, 0);
    for change in diff.iter_all_changes() {
        match change.tag() {
            similar::ChangeTag::Insert => added += 1,
            similar::ChangeTag::Delete => removed += 1,
            similar::ChangeTag::Equal => {}
        }
    }
    format!("+{added} -{removed}")
}

/// `text` fenced for a chat and cut to `limit` characters, with a note
/// giving how many lines were left out.
#[must_use]
pub fn for_chat(text: &str, limit: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let budget = limit.saturating_sub(80);
    let mut kept = String::new();
    let mut shown = 0;
    for line in &lines {
        if kept.len() + line.len() + 1 > budget {
            break;
        }
        kept.push_str(line);
        kept.push('\n');
        shown += 1;
    }
    let omitted = lines.len() - shown;
    if omitted == 0 {
        format!("```diff\n{kept}```")
    } else {
        format!("```diff\n{kept}```\n({omitted} more lines omitted)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_parse() {
        assert_eq!(Request::parse(""), Request::default());
        let parsed = Request::parse("session --stat src/");
        assert_eq!(parsed.mode, Mode::Session);
        assert!(parsed.stat);
        assert_eq!(parsed.paths, ["src/"]);
    }

    #[test]
    fn chat_output_is_fenced_and_cut() {
        let text = (0..100).fold(String::new(), |mut text, n| {
            let _ = writeln!(text, "+line {n}");
            text
        });
        let cut = for_chat(&text, 300);
        assert!(cut.starts_with("```diff\n+line 0\n"));
        assert!(cut.len() <= 300);
        assert!(cut.ends_with("more lines omitted)"), "{cut}");
        assert_eq!(for_chat("+a\n", 300), "```diff\n+a\n```");
    }
}

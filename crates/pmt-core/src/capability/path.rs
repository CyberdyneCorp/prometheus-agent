//! Path globs of `fs.read` / `fs.write`: syntax, the "covers" relation
//! between two globs, and matching a concrete path. Paths are compared
//! in canonical form (symlinks resolved), so `<workspace>/link/passwd`
//! with `link -> /etc` is checked as `/etc/passwd`.
//!
//! A glob without wildcards names that path and everything below it, and
//! `<dir>/**` names `<dir>` and everything below it: the same meaning the
//! OS sandbox gives a granted root (`pmt-sandbox`), so the host check and
//! the OS enforcement agree.

use std::path::{Component, Path, PathBuf};

/// The directories behind the glob placeholders `~`, `<workspace>`,
/// `<scratch>` and `<toolchain>`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Places {
    /// `~`.
    pub user_home: PathBuf,
    /// `<workspace>`.
    pub workspace: PathBuf,
    /// `<scratch>` (each of the session's scratch and spill directories).
    pub scratch: Vec<PathBuf>,
    /// `<toolchain>` roots.
    pub toolchain: Vec<PathBuf>,
}

/// Which `fs` permission a path check needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Reading (`fs.read` or `fs.write`).
    Read,
    /// Writing (`fs.write`).
    Write,
}

const TOOLCHAIN: &str = "<toolchain>";
const PLACEHOLDERS: &[&str] = &["<workspace>", "<scratch>"];

fn has_wildcard(text: &str) -> bool {
    text.contains(['*', '?', '['])
}

/// The canonical spelling of a glob, or `None` when it is not one: `/`
/// separators, no trailing `/`, no `.`/`..` segment (it could climb out of
/// a granted root), and an absolute path or a placeholder start.
pub(super) fn normalize_glob(text: &str) -> Option<String> {
    let mut glob = text.replace('\\', "/");
    while glob.len() > 1 && glob.ends_with('/') && !is_drive_root(&glob) {
        glob.pop();
    }
    if glob == TOOLCHAIN {
        return Some(glob);
    }
    let anchored = glob == "~"
        || glob.starts_with("~/")
        || glob.starts_with('/')
        || is_drive_absolute(&glob)
        || PLACEHOLDERS
            .iter()
            .any(|p| glob == *p || glob.starts_with(&format!("{p}/")));
    let clean = glob.split('/').all(|seg| seg != ".." && seg != ".");
    (anchored && clean && glob::Pattern::new(&glob).is_ok()).then_some(glob)
}

fn is_drive_absolute(glob: &str) -> bool {
    let bytes = glob.as_bytes();
    bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'/'
}

fn is_drive_root(glob: &str) -> bool {
    glob.len() == 3 && is_drive_absolute(glob)
}

/// The root a glob names with everything below it, when it is literal up
/// to an optional trailing `/**`.
fn subtree(glob: &str) -> Option<&str> {
    let base = glob.strip_suffix("/**").unwrap_or(glob);
    (!has_wildcard(base)).then_some(base)
}

/// Whether glob `a` provably includes every path of glob `b` (both in
/// canonical spelling; placeholders are compared as written).
pub(super) fn glob_covers(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    if a == TOOLCHAIN || b == TOOLCHAIN {
        return false;
    }
    let Some(base) = subtree(a) else {
        return false;
    };
    // `/**` strips to the empty root: every absolute glob is below it.
    let prefix = if base.ends_with('/') {
        base.to_owned()
    } else {
        format!("{base}/")
    };
    b == base || b.starts_with(&prefix)
}

impl Places {
    /// The absolute patterns `glob` stands for (one per scratch directory
    /// or toolchain root), with canonical bases.
    fn expand(&self, glob: &str) -> Vec<String> {
        let base = |dir: &Path, rest: &str| {
            let dir = slashed(&canonical(dir));
            format!("{}{rest}", glob::Pattern::escape(&dir))
        };
        if glob == TOOLCHAIN {
            return self
                .toolchain
                .iter()
                .map(|root| base(root, "/**"))
                .collect();
        }
        if let Some(rest) = glob.strip_prefix("<workspace>") {
            return vec![base(&self.workspace, rest)];
        }
        if let Some(rest) = glob.strip_prefix("<scratch>") {
            return self.scratch.iter().map(|dir| base(dir, rest)).collect();
        }
        if let Some(rest) = glob.strip_prefix('~') {
            return vec![base(&self.user_home, rest)];
        }
        vec![glob.to_owned()]
    }

    /// Whether the canonical `path` is inside `glob`.
    pub(super) fn matches(&self, glob: &str, path: &Path) -> bool {
        let path = slashed(path);
        self.expand(glob)
            .iter()
            .any(|pattern| pattern_matches(pattern, &path))
    }
}

fn pattern_matches(pattern: &str, path: &str) -> bool {
    if let Some(root) = subtree(pattern) {
        return root.is_empty() || Path::new(path).starts_with(root);
    }
    let options = glob::MatchOptions {
        case_sensitive: true,
        require_literal_separator: true,
        require_literal_leading_dot: false,
    };
    let matches =
        |pattern: &str| glob::Pattern::new(pattern).is_ok_and(|p| p.matches_with(path, options));
    // `<dir>/**` names `<dir>` too, also when `<dir>` is not literal (an
    // escaped run-time directory name); `subtree` took the literal case.
    matches(pattern) || pattern.strip_suffix("/**").is_some_and(matches)
}

/// The glob naming `dir` and everything below it: canonical, `/`
/// separators, wildcard characters escaped so an odd directory name is
/// taken literally.
pub(super) fn subtree_glob(dir: &Path) -> String {
    let dir = glob::Pattern::escape(&slashed(&canonical(dir)));
    format!("{}/**", dir.trim_end_matches('/'))
}

fn slashed(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// `path` resolved component by component: every existing prefix with its
/// symlinks resolved and the missing components kept as written; `..`
/// applies to the resolved prefix, as the OS would apply it. Windows
/// verbatim prefixes (`\\?\C:\`) are dropped so paths compare with
/// configured ones.
#[must_use]
pub fn canonical(path: &Path) -> PathBuf {
    let mut resolved = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                resolved.pop();
            }
            Component::CurDir => {}
            other => {
                resolved.push(other.as_os_str());
                // Resolve every prefix that exists, including one reached
                // again through `..` after a missing component.
                if let Ok(real) = std::fs::canonicalize(&resolved) {
                    resolved = strip_verbatim(real);
                }
            }
        }
    }
    resolved
}

#[cfg(windows)]
fn strip_verbatim(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => PathBuf::from(rest),
        _ => path,
    }
}

#[cfg(not(windows))]
fn strip_verbatim(path: PathBuf) -> PathBuf {
    path
}

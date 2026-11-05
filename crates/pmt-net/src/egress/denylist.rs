//! The profile's website blocklist (spec: security-boundaries, Website
//! blocklist): host patterns refused in every egress mode, from
//! `security.egress.deny` and from shared list files
//! (`security.egress.deny_files`: one pattern per line, `#` comments).
//!
//! A list file is re-read when its size or modification time changes, so
//! an administrator's edit reaches running sessions at their next
//! connection. A file that cannot be read contributes nothing and is
//! reported once at WARN: a missing shared list never cuts every session
//! off the network.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::SystemTime;

use super::policy::Allowed;

/// One deny pattern: a host glob (`*` matches any run of characters,
/// dots included) and an optional port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostGlob(Allowed);

impl HostGlob {
    /// Parses `host[:port]`, where the host may hold `*` globs.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        Self(Allowed::parse(text))
    }

    /// Whether this pattern names `host:port` (`host` lower case).
    #[must_use]
    pub fn matches(&self, host: &str, port: u16) -> bool {
        glob(&self.0.host, host) && self.0.port.is_none_or(|p| p == port)
    }
}

impl std::fmt::Display for HostGlob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// `*` matches any run of characters (single-star backtracking).
fn glob(pattern: &str, text: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pattern.chars().collect(), text.chars().collect());
    let (mut pi, mut ti) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

/// What a list file held when it was last read.
#[derive(Debug, Default)]
struct Loaded {
    stamp: Option<(SystemTime, u64)>,
    patterns: Vec<HostGlob>,
}

/// The profile's blocklist: configured patterns plus list files.
#[derive(Debug, Default)]
pub struct Denylist {
    patterns: Vec<HostGlob>,
    files: Vec<(PathBuf, Mutex<Loaded>)>,
    warned: Mutex<HashSet<PathBuf>>,
}

impl Denylist {
    /// The list of `patterns` and the patterns in `files`.
    #[must_use]
    pub fn new(patterns: &[String], files: &[PathBuf]) -> Self {
        Self {
            patterns: patterns.iter().map(|p| HostGlob::parse(p)).collect(),
            files: files
                .iter()
                .map(|f| (f.clone(), Mutex::default()))
                .collect(),
            warned: Mutex::default(),
        }
    }

    /// Whether the list has nothing to check.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty() && self.files.is_empty()
    }

    /// The pattern refusing `host:port` (case and a trailing dot are
    /// ignored), if any.
    #[must_use]
    pub fn matching(&self, host: &str, port: u16) -> Option<String> {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        if let Some(hit) = self.patterns.iter().find(|p| p.matches(&host, port)) {
            return Some(hit.to_string());
        }
        self.files.iter().find_map(|(path, loaded)| {
            let mut loaded = loaded.lock().unwrap_or_else(PoisonError::into_inner);
            self.refresh(path, &mut loaded);
            loaded
                .patterns
                .iter()
                .find(|p| p.matches(&host, port))
                .map(ToString::to_string)
        })
    }

    /// Every pattern in force now (configured, then each file's).
    #[must_use]
    pub fn patterns(&self) -> Vec<String> {
        let mut all: Vec<String> = self.patterns.iter().map(ToString::to_string).collect();
        for (path, loaded) in &self.files {
            let mut loaded = loaded.lock().unwrap_or_else(PoisonError::into_inner);
            self.refresh(path, &mut loaded);
            all.extend(loaded.patterns.iter().map(ToString::to_string));
        }
        all
    }

    /// Re-reads `path` when its size or modification time changed.
    fn refresh(&self, path: &Path, loaded: &mut Loaded) {
        let stamp = std::fs::metadata(path)
            .ok()
            .map(|m| (m.modified().unwrap_or(SystemTime::UNIX_EPOCH), m.len()));
        if stamp.is_some() && stamp == loaded.stamp {
            return;
        }
        loaded.stamp = stamp;
        loaded.patterns = match std::fs::read_to_string(path) {
            Ok(text) => parse_file(&text),
            Err(err) => {
                self.warn_once(path, &err);
                Vec::new()
            }
        };
    }

    fn warn_once(&self, path: &Path, err: &std::io::Error) {
        let mut warned = self.warned.lock().unwrap_or_else(PoisonError::into_inner);
        if warned.insert(path.to_owned()) {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "security.egress.deny_files: cannot read the list; its patterns are not applied"
            );
        }
    }
}

/// One pattern per line; `#` starts a comment.
fn parse_file(text: &str) -> Vec<HostGlob> {
    text.lines()
        .map(|line| line.split('#').next().unwrap_or_default().trim())
        .filter(|line| !line.is_empty())
        .map(HostGlob::parse)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{Denylist, HostGlob};

    #[test]
    fn globs_match_any_run_of_characters() {
        let casino = HostGlob::parse("*.casino.example");
        assert!(casino.matches("play.casino.example", 443));
        assert!(casino.matches("a.b.casino.example", 80));
        assert!(!casino.matches("casino.example", 443));
        assert!(HostGlob::parse("ads*.example").matches("ads7.example", 443));
        assert!(!HostGlob::parse("evil.example:8080").matches("evil.example", 443));
    }

    #[test]
    fn a_list_file_is_reread_when_it_changes() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("blocked.txt");
        std::fs::write(&file, "# shared list\n*.casino.example  # gambling\n\n").unwrap();
        let list = Denylist::new(&["tracker.example".into()], std::slice::from_ref(&file));
        assert_eq!(
            list.matching("Play.Casino.Example.", 443).as_deref(),
            Some("*.casino.example")
        );
        assert_eq!(
            list.matching("tracker.example", 80).as_deref(),
            Some("tracker.example")
        );
        assert_eq!(list.matching("example.org", 443), None);
        std::fs::write(&file, "example.org\nmore.example.org\n").unwrap();
        assert_eq!(
            list.matching("example.org", 443).as_deref(),
            Some("example.org")
        );
        assert_eq!(list.matching("play.casino.example", 443), None);
    }

    #[test]
    fn an_unreadable_file_blocks_nothing() {
        let list = Denylist::new(&[], &["/nonexistent/prometheus/blocked.txt".into()]);
        assert_eq!(list.matching("example.org", 443), None);
    }
}

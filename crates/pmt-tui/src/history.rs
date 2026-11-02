//! The composer's history (spec: tui, Composer and path completion): per
//! profile in `<profile>/tui/history`, at most [`MAX_ENTRIES`] entries,
//! owner-only, credential-shaped text redacted before it is kept. One
//! entry per line; a backslash and a line break inside an entry are
//! written as `\\` and `\n`.

use std::io;
use std::path::{Path, PathBuf};

use pmt_core::secret::Redactor;

/// Entries kept.
pub const MAX_ENTRIES: usize = 1000;

/// The history file of a profile directory.
#[must_use]
pub fn path_in(profile_dir: &Path) -> PathBuf {
    profile_dir.join("tui").join("history")
}

/// The entries and the place being browsed.
#[derive(Debug, Clone)]
pub struct History {
    path: PathBuf,
    entries: Vec<String>,
    /// The entry shown while browsing (`None`: the draft).
    browsing: Option<usize>,
    /// What was typed before browsing started.
    draft: String,
    redactor: Redactor,
}

fn escape(entry: &str) -> String {
    entry.replace('\\', "\\\\").replace('\n', "\\n")
}

fn unescape(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match (c, c == '\\') {
            (_, true) => match chars.next() {
                Some('n') => out.push('\n'),
                Some(other) => out.push(other),
                None => out.push('\\'),
            },
            (c, false) => out.push(c),
        }
    }
    out
}

impl History {
    /// The history in `path` (empty when the file does not exist or cannot
    /// be read).
    #[must_use]
    pub fn load(path: PathBuf, redactor: Redactor) -> Self {
        let entries = std::fs::read_to_string(&path)
            .map(|text| text.lines().map(unescape).collect::<Vec<_>>())
            .unwrap_or_default();
        let skip = entries.len().saturating_sub(MAX_ENTRIES);
        Self {
            path,
            entries: entries.into_iter().skip(skip).collect(),
            browsing: None,
            draft: String::new(),
            redactor,
        }
    }

    /// The kept entries, oldest first.
    #[must_use]
    pub fn entries(&self) -> &[String] {
        &self.entries
    }

    /// Keeps `entry` (redacted; a repeat of the newest is not kept twice)
    /// and rewrites the file.
    ///
    /// # Errors
    ///
    /// The file cannot be written.
    pub fn push(&mut self, entry: &str) -> io::Result<()> {
        self.browsing = None;
        let entry = self.redactor.redact_all(entry);
        if entry.trim().is_empty() || self.entries.last() == Some(&entry) {
            return Ok(());
        }
        self.entries.push(entry);
        if self.entries.len() > MAX_ENTRIES {
            self.entries.remove(0);
        }
        if let Some(dir) = self.path.parent() {
            pmt_core::private_fs::create_dir(dir)?;
        }
        let mut text = String::new();
        for entry in &self.entries {
            text.push_str(&escape(entry));
            text.push('\n');
        }
        pmt_core::private_fs::write_atomic(&self.path, text.as_bytes())
    }

    /// The entry before the one shown (`current` is the composer's text,
    /// kept as the draft when browsing starts).
    pub fn previous(&mut self, current: &str) -> Option<&str> {
        let at = match self.browsing {
            None if self.entries.is_empty() => return None,
            None => {
                current.clone_into(&mut self.draft);
                self.entries.len() - 1
            }
            Some(0) => 0,
            Some(at) => at - 1,
        };
        self.browsing = Some(at);
        self.entries.get(at).map(String::as_str)
    }

    /// The entry after the one shown, then the draft.
    pub fn following(&mut self) -> Option<&str> {
        let at = self.browsing?;
        if at + 1 < self.entries.len() {
            self.browsing = Some(at + 1);
            return self.entries.get(at + 1).map(String::as_str);
        }
        self.browsing = None;
        Some(&self.draft)
    }
}

#[cfg(test)]
mod tests {
    use pmt_core::secret::Redactor;

    use super::{History, MAX_ENTRIES, escape, path_in, unescape};

    #[test]
    fn entries_survive_a_restart_capped_and_redacted() {
        let dir = tempfile::tempdir().unwrap();
        let path = path_in(dir.path());
        let mut history = History::load(path.clone(), Redactor::default());
        for n in 0..=MAX_ENTRIES {
            history.push(&format!("prompt {n}")).unwrap();
        }
        history
            .push("deploy with sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123")
            .unwrap();
        history.push("two\nlines \\ here").unwrap();
        let reloaded = History::load(path, Redactor::default());
        assert_eq!(reloaded.entries().len(), MAX_ENTRIES);
        assert_eq!(reloaded.entries()[0], "prompt 3", "the oldest went first");
        let secret = &reloaded.entries()[MAX_ENTRIES - 2];
        assert!(!secret.contains("abcdefghijklmnop"), "{secret}");
        assert_eq!(reloaded.entries()[MAX_ENTRIES - 1], "two\nlines \\ here");
    }

    #[test]
    fn browsing_walks_back_and_returns_to_the_draft() {
        let dir = tempfile::tempdir().unwrap();
        let mut history = History::load(path_in(dir.path()), Redactor::default());
        history.push("first").unwrap();
        history.push("second").unwrap();
        assert_eq!(history.previous("draft"), Some("second"));
        assert_eq!(history.previous(""), Some("first"));
        assert_eq!(history.previous(""), Some("first"), "stays at the oldest");
        assert_eq!(history.following(), Some("second"));
        assert_eq!(history.following(), Some("draft"));
        assert_eq!(history.following(), None);
    }

    #[test]
    fn escaping_round_trips() {
        for text in ["a\\nb", "line\nbreak", "\\", "plain"] {
            assert_eq!(unescape(&escape(text)), text);
        }
    }
}

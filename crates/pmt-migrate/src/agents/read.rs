//! Reading another agent's directory: only through these helpers, which
//! open files for reading and remember every file read so a sync can tell
//! by digest whether the source changed (spec: hermes-migration, Keeping
//! imports in sync). Credential files are never passed to them.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::ledger;

/// The files of one source, read once.
pub struct Reader {
    root: PathBuf,
    read: RefCell<BTreeSet<PathBuf>>,
}

impl Reader {
    /// A reader of the source at `root`.
    #[must_use]
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_owned(),
            read: RefCell::new(BTreeSet::new()),
        }
    }

    /// The source directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The path of `rel` in the source.
    #[must_use]
    pub fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    /// The text of `path` (`None` when missing); an unreadable file is
    /// `Err(reason)`.
    ///
    /// # Errors
    ///
    /// The I/O error as text.
    pub fn text(&self, path: &Path) -> Result<Option<String>, String> {
        match std::fs::read(path) {
            Ok(bytes) => {
                self.read.borrow_mut().insert(path.to_owned());
                Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("cannot read {}: {e}", path.display())),
        }
    }

    /// `path` parsed as JSON (comments and trailing commas allowed, as
    /// the agents' JSON5-style files have them).
    ///
    /// # Errors
    ///
    /// The read or parse error as text.
    pub fn json(&self, path: &Path) -> Result<Option<Value>, String> {
        let Some(text) = self.text(path)? else {
            return Ok(None);
        };
        serde_json::from_str(&relax(&text))
            .map(Some)
            .map_err(|e| format!("{} is not valid JSON: {e}", path.display()))
    }

    /// `path` parsed as TOML, as JSON values.
    ///
    /// # Errors
    ///
    /// The read or parse error as text.
    pub fn toml(&self, path: &Path) -> Result<Option<Value>, String> {
        let Some(text) = self.text(path)? else {
            return Ok(None);
        };
        let table: toml::Table = toml::from_str(&text)
            .map_err(|e| format!("{} is not valid TOML: {}", path.display(), e.message()))?;
        serde_json::to_value(table)
            .map(Some)
            .map_err(|e| format!("{}: {e}", path.display()))
    }

    /// The files directly in `dir` with one of `extensions`, sorted.
    #[must_use]
    pub fn files(&self, dir: &Path, extensions: &[&str]) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.is_file())
                    .filter(|p| {
                        p.extension()
                            .and_then(|e| e.to_str())
                            .is_some_and(|e| extensions.contains(&e))
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.sort();
        out
    }

    /// The skill directories (holding a `SKILL.md`) directly below `dir`,
    /// or at any depth when `deep`; their files count as read.
    #[must_use]
    pub fn skill_dirs(&self, dir: &Path, deep: bool) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let mut stack = vec![dir.to_owned()];
        while let Some(current) = stack.pop() {
            for path in subdirs(&current) {
                if path.join("SKILL.md").is_file() {
                    found.push(path);
                } else if deep {
                    stack.push(path);
                }
            }
        }
        found.sort();
        for skill in &found {
            for rel in pmt_skills::layout::walk(skill) {
                self.read.borrow_mut().insert(skill.join(rel));
            }
        }
        found
    }

    /// A digest of every file read (paths relative to the source and
    /// their bytes as they are now).
    #[must_use]
    pub fn digest(&self) -> String {
        let mut bytes = Vec::new();
        for path in self.read.borrow().iter() {
            let rel = path.strip_prefix(&self.root).unwrap_or(path);
            bytes.extend_from_slice(rel.to_string_lossy().as_bytes());
            bytes.push(0);
            bytes.extend_from_slice(&std::fs::read(path).unwrap_or_default());
            bytes.push(0);
        }
        ledger::digest(&bytes)
    }
}

/// `text` with `//` and `/* */` comments and trailing commas removed
/// (outside strings), so JSON5-style config files parse as JSON.
#[must_use]
pub fn relax(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        let next = chars.peek().copied();
        match (c, next) {
            ('"', _) => string(c, &mut chars, &mut out),
            ('/', Some('/')) => {
                if chars.any(|n| n == '\n') {
                    out.push('\n');
                }
            }
            ('/', Some('*')) => block_comment(&mut chars),
            (',', _) if closes(chars.clone()) => {}
            _ => out.push(c),
        }
    }
    out
}

type Chars<'a> = std::iter::Peekable<std::str::Chars<'a>>;

/// Copies a string literal opened by `quote`, escapes included.
fn string(quote: char, chars: &mut Chars<'_>, out: &mut String) {
    out.push(quote);
    while let Some(c) = chars.next() {
        out.push(c);
        match c {
            '\\' => out.extend(chars.next()),
            '"' => return,
            _ => {}
        }
    }
}

/// Skips a `/* */` comment (its opening `/` already read).
fn block_comment(chars: &mut Chars<'_>) {
    chars.next();
    let mut last = ' ';
    for n in chars.by_ref() {
        if last == '*' && n == '/' {
            return;
        }
        last = n;
    }
}

/// Whether the next non-blank character closes an object or array (the
/// comma before it is a trailing comma).
fn closes(mut rest: Chars<'_>) -> bool {
    matches!(rest.find(|c| !c.is_whitespace()), Some('}' | ']'))
}

/// The paragraphs of a Markdown text (blank-line separated, a heading
/// kept with the paragraph that follows it).
#[must_use]
pub fn paragraphs(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut heading: Option<String> = None;
    for block in text.replace("\r\n", "\n").split("\n\n") {
        let block = block.trim();
        if block.is_empty() {
            continue;
        }
        let only_heading = block.lines().count() == 1 && block.starts_with('#');
        if only_heading {
            if let Some(h) = heading.take() {
                out.push(h);
            }
            heading = Some(block.to_owned());
            continue;
        }
        out.push(match heading.take() {
            Some(h) => format!("{h}\n{block}"),
            None => block.to_owned(),
        });
    }
    out.extend(heading);
    out
}

/// `text` without a leading `---` front matter block.
#[must_use]
pub fn strip_frontmatter(text: &str) -> &str {
    let Some(rest) = text.strip_prefix("---") else {
        return text;
    };
    let Some(end) = rest.find("\n---") else {
        return text;
    };
    let after = &rest[end + 4..];
    after.strip_prefix('\n').unwrap_or(after)
}

/// The visible subdirectories of `dir`.
fn subdirs(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json5_comments_and_trailing_commas_are_relaxed() {
        let text =
            "{\n // a comment\n \"a\": \"x // not a comment\", /* block */\n \"b\": [1, 2,],\n}";
        let v: Value = serde_json::from_str(&relax(text)).unwrap();
        assert_eq!(v["a"], "x // not a comment");
        assert_eq!(v["b"], serde_json::json!([1, 2]));
    }

    #[test]
    fn paragraphs_keep_headings_with_their_text() {
        assert_eq!(
            paragraphs("# Style\n\nUse tabs.\n\nNo emoji.\n"),
            ["# Style\nUse tabs.", "No emoji."]
        );
    }

    #[test]
    fn front_matter_is_stripped() {
        assert_eq!(
            strip_frontmatter("---\nglobs: *.rs\nalwaysApply: true\n---\nPrefer iterators.\n"),
            "Prefer iterators.\n"
        );
        assert_eq!(strip_frontmatter("plain"), "plain");
    }
}

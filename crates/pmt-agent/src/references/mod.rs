//! `@` context references in user input (spec: agent-turn-loop, Context
//! references in user input; Context reference safety and completion).
//!
//! On the CLI, TUI, desktop and ACP surfaces a message may name context
//! to attach: `@file:<path>[:<line>|:<a>-<b>]`, `@folder:<path>`, `@diff`,
//! `@staged`, `@git:<N>` and `@url:<url>`. The content is appended under
//! `--- Attached Context ---`; every failure becomes an inline warning, and
//! an expansion above half the context window is dropped entirely.
//! Messaging adapters never expand (their users are not the machine's
//! owner).

pub mod complete;
pub mod safety;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

pub use safety::Places;

use crate::turn::model::BoxFuture;

/// The heading of the attachments.
pub const ATTACHED: &str = "--- Attached Context ---";

/// Folder listings stop after this many entries.
pub const MAX_FOLDER_ENTRIES: usize = 200;

/// One reference found in a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reference {
    /// `@file:<path>` with an optional 1-based inclusive line range.
    File {
        /// The path as written.
        path: String,
        /// First and last line.
        lines: Option<(usize, usize)>,
    },
    /// `@folder:<path>`.
    Folder(String),
    /// `@diff`: unstaged changes.
    Diff,
    /// `@staged`: staged changes.
    Staged,
    /// `@git:<N>`: the last N commits with patches (1..=10).
    Git(usize),
    /// `@url:<url>`.
    Url(String),
}

impl Reference {
    /// The reference as written back in labels.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::File { path, lines: None } => format!("@file:{path}"),
            Self::File {
                path,
                lines: Some((a, b)),
            } if a == b => format!("@file:{path}:{a}"),
            Self::File {
                path,
                lines: Some((a, b)),
            } => format!("@file:{path}:{a}-{b}"),
            Self::Folder(path) => format!("@folder:{path}"),
            Self::Diff => "@diff".to_owned(),
            Self::Staged => "@staged".to_owned(),
            Self::Git(n) => format!("@git:{n}"),
            Self::Url(url) => format!("@url:{url}"),
        }
    }
}

/// Every reference of `text`, in order. A reference starts a word.
#[must_use]
pub fn parse(text: &str) -> Vec<Reference> {
    text.split_whitespace()
        .filter_map(|word| word.strip_prefix('@'))
        .filter_map(|word| parse_one(word.trim_end_matches([',', '.', ';', '!', '?'])))
        .collect()
}

fn parse_one(word: &str) -> Option<Reference> {
    match word {
        "diff" => return Some(Reference::Diff),
        "staged" => return Some(Reference::Staged),
        _ => {}
    }
    let (kind, value) = word.split_once(':')?;
    if value.is_empty() {
        return None;
    }
    match kind {
        "file" => Some(file_reference(value)),
        "folder" => Some(Reference::Folder(value.to_owned())),
        "git" => value
            .parse::<usize>()
            .ok()
            .map(|n| Reference::Git(n.clamp(1, 10))),
        "url" => Some(Reference::Url(value.to_owned())),
        _ => None,
    }
}

/// `path[:line|:a-b]`; an invalid range means the whole file.
fn file_reference(value: &str) -> Reference {
    let whole = || Reference::File {
        path: value.to_owned(),
        lines: None,
    };
    let Some((path, range)) = value.rsplit_once(':') else {
        return whole();
    };
    if path.is_empty() || !range.chars().all(|c| c.is_ascii_digit() || c == '-') {
        return whole(); // `C:\x` and other colons in names
    }
    let lines = match range.split_once('-') {
        Some((a, b)) => a.parse().ok().zip(b.parse().ok()),
        None => range.parse().ok().map(|n| (n, n)),
    };
    Reference::File {
        path: path.to_owned(),
        lines: lines.filter(|&(a, b)| a >= 1 && a <= b),
    }
}

/// Fetches `@url:` content through the session's web extraction path.
pub trait UrlSource: Send + Sync {
    /// The page text, or why it could not be fetched.
    fn extract<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Result<String, String>>;
}

impl UrlSource for pmt_tools::web::WebSession {
    fn extract<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            let result = pmt_tools::web::WebSession::extract(self, &[url.to_owned()], None).await;
            let text = result.text_content();
            if result.status == pmt_tools::Status::Ok {
                Ok(text)
            } else {
                Err(text)
            }
        })
    }
}

/// What expansion needs.
pub struct Expander<'a> {
    /// Where paths resolve and what they may reach.
    pub places: &'a Places,
    /// The main model's context window in tokens.
    pub context_window: u64,
    /// `@url:` fetching; `None` turns URL references into warnings.
    pub urls: Option<&'a dyn UrlSource>,
}

/// An expanded message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expanded {
    /// The message to send and store.
    pub text: String,
    /// Warnings for the user (also inline in `text` unless refused).
    pub warnings: Vec<String>,
}

impl Expander<'_> {
    /// Expands every reference of `text`.
    pub async fn expand(&self, text: &str) -> Expanded {
        let references = parse(text);
        if references.is_empty() {
            return Expanded {
                text: text.to_owned(),
                warnings: Vec::new(),
            };
        }
        let mut attached = String::new();
        let mut warnings = Vec::new();
        for reference in &references {
            match self.attach(reference).await {
                Ok(body) => {
                    let _ = write!(attached, "\n\n### {}\n{body}", reference.label());
                }
                Err(why) => warnings.push(format!("{}: {why}", reference.label())),
            }
        }
        self.assemble(text, &attached, warnings)
    }

    fn assemble(&self, text: &str, attached: &str, mut warnings: Vec<String>) -> Expanded {
        let tokens = u64::try_from(attached.len().div_ceil(4)).unwrap_or(u64::MAX);
        if tokens.saturating_mul(2) > self.context_window {
            let notice = format!(
                "context references exceed 50% of the context window ({tokens} of {} tokens); the message was sent without them",
                self.context_window
            );
            return Expanded {
                text: text.to_owned(),
                warnings: vec![notice],
            };
        }
        if tokens.saturating_mul(4) > self.context_window {
            warnings.push(format!(
                "attached context uses more than 25% of the context window ({tokens} of {} tokens)",
                self.context_window
            ));
        }
        let mut out = format!("{text}\n\n{ATTACHED}{attached}");
        for warning in &warnings {
            let _ = write!(out, "\n\n[warning] {warning}");
        }
        Expanded {
            text: out,
            warnings,
        }
    }

    async fn attach(&self, reference: &Reference) -> Result<String, String> {
        match reference {
            Reference::File { path, lines } => self.file(path, *lines),
            Reference::Folder(path) => self.folder(path),
            Reference::Diff => git(&self.places.cwd, &["diff"]),
            Reference::Staged => git(&self.places.cwd, &["diff", "--staged"]),
            Reference::Git(n) => git(&self.places.cwd, &["log", &format!("-{n}"), "-p"]),
            Reference::Url(url) => match self.urls {
                Some(urls) => urls.extract(url).await,
                None => Err("web extraction is not available in this session".to_owned()),
            },
        }
    }

    fn checked(&self, raw: &str) -> Result<PathBuf, String> {
        let path = self.places.resolve(raw);
        match self.places.refusal(&path) {
            Some(why) => Err(why.to_owned()),
            None => Ok(path),
        }
    }

    fn file(&self, raw: &str, lines: Option<(usize, usize)>) -> Result<String, String> {
        let path = self.checked(raw)?;
        let bytes = std::fs::read(&path).map_err(|e| format!("cannot read: {e}"))?;
        if safety::is_binary(&path, &bytes[..bytes.len().min(8192)]) {
            return Err(safety::BINARY.to_owned());
        }
        let text = String::from_utf8_lossy(&bytes);
        let body = match lines {
            Some((a, b)) => text
                .lines()
                .skip(a - 1)
                .take(b - a + 1)
                .collect::<Vec<_>>()
                .join("\n"),
            None => text.trim_end().to_owned(),
        };
        Ok(format!("```\n{body}\n```"))
    }

    fn folder(&self, raw: &str) -> Result<String, String> {
        let path = self.checked(raw)?;
        if !path.is_dir() {
            return Err("not a folder".to_owned());
        }
        let mut lines = Vec::new();
        let complete = list(&path, 0, &mut lines);
        if !complete {
            lines.push("- ...".to_owned());
        }
        Ok(lines.join("\n"))
    }
}

/// Appends a tree listing of `dir` to `lines`; `false` once the entry cap
/// is reached.
fn list(dir: &Path, depth: usize, lines: &mut Vec<String>) -> bool {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|e| e.file_name() != ".git")
        .collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    let indent = "  ".repeat(depth);
    for entry in entries {
        if lines.len() >= MAX_FOLDER_ENTRIES {
            return false;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            lines.push(format!("{indent}- {name}/"));
            if !list(&entry.path(), depth + 1, lines) {
                return false;
            }
        } else {
            lines.push(format!("{indent}- {name} ({})", complete::size(meta.len())));
        }
    }
    true
}

fn git(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .map_err(|e| format!("git could not run: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    if text.trim().is_empty() {
        return Ok("(no changes)".to_owned());
    }
    Ok(format!("```diff\n{}\n```", text.trim_end()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references_parse_with_ranges_and_punctuation() {
        let refs = parse(
            "see @file:src/main.rs:10-20, @folder:src. and @diff! @git:99 @url:https://x.dev/a?b",
        );
        assert_eq!(
            refs,
            [
                Reference::File {
                    path: "src/main.rs".into(),
                    lines: Some((10, 20))
                },
                Reference::Folder("src".into()),
                Reference::Diff,
                Reference::Git(10),
                Reference::Url("https://x.dev/a?b".into()),
            ]
        );
        assert_eq!(
            parse("@file:a.rs:20-10 @file:b.rs:7 a@file:x"),
            [
                Reference::File {
                    path: "a.rs".into(),
                    lines: None
                },
                Reference::File {
                    path: "b.rs".into(),
                    lines: Some((7, 7))
                },
            ]
        );
        assert_eq!(
            parse("@git:0 @staged @other:x"),
            [Reference::Git(1), Reference::Staged]
        );
    }
}

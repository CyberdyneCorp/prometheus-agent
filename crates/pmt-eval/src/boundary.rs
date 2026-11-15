//! The answer-free boundary (spec: replay-and-evaluation, Answer-free
//! boundary): grader material never reaches a task session — it is never
//! copied into a workspace and the session's sandbox only reads its
//! workspace — and a task whose trajectory shows an attempt to read a
//! path inside a `grader/` directory, or a path matching the suite's
//! `leak_globs`, is `contaminated` and excluded from scoring.
//!
//! [`Scanner`] looks at every tool call's arguments and result: a path
//! with a `grader` directory component, a path inside one of the suite's
//! grader directories, or a word matching a leak glob is an attempt.

use std::path::{Path, PathBuf};

/// One tool call of a task's trajectory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolUse {
    /// The tool.
    pub tool: String,
    /// Its arguments as the model sent them.
    pub arguments: String,
    /// What it returned.
    pub result: String,
}

/// Finds attempts on grader material in a trajectory.
#[derive(Debug, Clone)]
pub struct Scanner {
    graders: Vec<String>,
    globs: Vec<glob::Pattern>,
}

impl Scanner {
    /// A scanner for the grader directories `graders` and the suite's
    /// `leak_globs` (invalid globs are ignored; suites are validated).
    #[must_use]
    pub fn new(graders: &[PathBuf], leak_globs: &[String]) -> Self {
        let mut dirs = Vec::new();
        for dir in graders {
            dirs.push(dir.to_string_lossy().into_owned());
            if let Ok(canonical) = dir.canonicalize() {
                dirs.push(canonical.to_string_lossy().into_owned());
            }
        }
        Self {
            graders: dirs,
            globs: leak_globs
                .iter()
                .filter_map(|g| glob::Pattern::new(g).ok())
                .collect(),
        }
    }

    /// The paths (or words) of every attempt in `uses`, in order, once
    /// each.
    #[must_use]
    pub fn attempts(&self, uses: &[ToolUse]) -> Vec<String> {
        let mut found = Vec::new();
        for text in uses
            .iter()
            .flat_map(|u| [u.arguments.as_str(), u.result.as_str()])
        {
            for hit in self.hits(text) {
                if !found.contains(&hit) {
                    found.push(hit);
                }
            }
        }
        found
    }

    fn hits(&self, text: &str) -> Vec<String> {
        let mut hits = Vec::new();
        for dir in &self.graders {
            if text.contains(dir.as_str()) {
                hits.push(dir.clone());
            }
        }
        for word in words(text) {
            if in_grader_dir(word) || self.leaks(word) {
                hits.push(word.to_owned());
            }
        }
        hits
    }

    fn leaks(&self, word: &str) -> bool {
        let path = Path::new(word);
        let name = path.file_name().map(Path::new);
        self.globs
            .iter()
            .any(|g| g.matches_path(path) || name.is_some_and(|n| g.matches_path(n)))
    }
}

/// The path-like words of `text`: split at whitespace, quotes and the
/// punctuation JSON and shell put around paths.
fn words(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| {
        c.is_whitespace()
            || matches!(
                c,
                '"' | '\'' | '`' | ',' | '=' | '(' | ')' | ';' | '|' | '{' | '}' | '[' | ']'
            )
    })
    .map(|w| w.trim_end_matches([':', '\\']))
    .filter(|w| !w.is_empty())
}

/// A path with a `grader` directory component followed by more path.
fn in_grader_dir(word: &str) -> bool {
    let parts: Vec<&str> = word.split(['/', '\\']).collect();
    parts
        .iter()
        .take(parts.len().saturating_sub(1))
        .any(|p| *p == "grader")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(arguments: &str, result: &str) -> ToolUse {
        ToolUse {
            tool: "terminal".to_owned(),
            arguments: arguments.to_owned(),
            result: result.to_owned(),
        }
    }

    #[test]
    fn grader_paths_and_leak_globs_are_attempts() {
        let scanner = Scanner::new(
            &[PathBuf::from("/s/tasks/t1/grader")],
            &["reference_answer*".to_owned()],
        );
        assert!(
            scanner
                .attempts(&[call(r#"{"command": "ls -la"}"#, "README.md")])
                .is_empty()
        );
        assert_eq!(
            scanner.attempts(&[call(
                r#"{"command": "find / -name \"reference_answer*\""}"#,
                ""
            )]),
            ["reference_answer*"]
        );
        assert_eq!(
            scanner.attempts(&[call(r#"{"path": "../grader/expected.txt"}"#, "")]),
            ["../grader/expected.txt"]
        );
        assert_eq!(
            scanner.attempts(&[call("{}", "/s/tasks/t1/grader/grader.toml")]),
            ["/s/tasks/t1/grader", "/s/tasks/t1/grader/grader.toml"]
        );
        // A file named `grader` is not a grader directory.
        assert!(
            scanner
                .attempts(&[call(r#"{"path": "src/grader"}"#, "")])
                .is_empty()
        );
    }
}

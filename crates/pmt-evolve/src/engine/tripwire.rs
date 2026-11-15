//! The leak tripwire (spec: harness-evolution, Leak globs and verdict
//! tripwire): no grader material may reach an improver, a selector, an
//! accepted harness or a library entry. A file is rejected when its path
//! matches a leak glob, or when its name or content carries a verdict
//! marker — the canary `pmt-eval-verdict` that eval verdict files hold, or
//! a `"pass_rate"`, `"passed_count"` or `"score"` key with a number.

use std::path::Path;
use std::sync::LazyLock;

use pmt_config::harness::Tree;
use regex::Regex;

use crate::job::task::LeakGlobs;

/// The canary every eval verdict file carries.
pub const VERDICT_CANARY: &str = "pmt-eval-verdict";

/// Characters quoted before a match.
const BEFORE: usize = 60;
/// Characters quoted after the start of a match.
const AFTER: usize = 120;

static SCORE_KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#""(pass_rate|passed_count|score)"\s*:\s*-?[0-9]"#).expect("valid regex")
});

/// The leak globs and verdict markers one job checks against.
#[derive(Debug, Clone)]
pub struct Tripwire {
    leaks: LeakGlobs,
}

impl Tripwire {
    /// A tripwire over `globs` (`evolve.leak_globs`).
    #[must_use]
    pub fn new(globs: &[String]) -> Self {
        Self {
            leaks: LeakGlobs::new(globs),
        }
    }

    /// Checks every file under `root` (paths relative to it).
    ///
    /// # Errors
    ///
    /// The first hit, quoted.
    pub fn check_dir(&self, root: &Path) -> Result<(), String> {
        let mut files = Vec::new();
        walk(root, root, &mut files);
        files.sort();
        for relative in files {
            self.check_path(&relative)?;
            let bytes = std::fs::read(root.join(&relative)).unwrap_or_default();
            check_text(&relative, &String::from_utf8_lossy(&bytes))?;
        }
        Ok(())
    }

    /// Checks a harness tree in memory.
    ///
    /// # Errors
    ///
    /// The first hit, quoted.
    pub fn check_tree(&self, tree: &Tree) -> Result<(), String> {
        for (path, text) in &tree.files {
            self.check_path(path)?;
            check_text(path, text)?;
        }
        Ok(())
    }

    /// Checks one relative path against the leak globs and the canary.
    ///
    /// # Errors
    ///
    /// The hit.
    pub fn check_path(&self, relative: &str) -> Result<(), String> {
        if self.leaks.matches(relative) {
            return Err(format!("tripwire: {relative} matches a leak glob"));
        }
        if let Some(at) = relative.find(VERDICT_CANARY) {
            return Err(hit(relative, "name", relative, at));
        }
        Ok(())
    }
}

/// Checks `text` (from `name`) for a verdict marker.
///
/// # Errors
///
/// The first marker with 60 characters before and 120 after it.
pub fn check_text(name: &str, text: &str) -> Result<(), String> {
    let at = text
        .find(VERDICT_CANARY)
        .or_else(|| SCORE_KEY.find(text).map(|m| m.start()));
    match at {
        Some(at) => Err(hit(name, "content", text, at)),
        None => Ok(()),
    }
}

fn hit(name: &str, what: &str, text: &str, at: usize) -> String {
    let start = floor(text, at.saturating_sub(BEFORE));
    let end = floor(text, (at + AFTER).min(text.len()));
    let quote: String = text[start..end].replace('\n', "\\n");
    format!("tripwire: verdict marker in the {what} of {name}: \"{quote}\"")
}

/// The largest char boundary at or below `i`.
fn floor(text: &str, mut i: usize) -> usize {
    while !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            walk(root, &path, out);
        } else if kind.is_file() {
            let relative = path.strip_prefix(root).unwrap_or(&path);
            out.push(relative.to_string_lossy().replace('\\', "/"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdict_markers_and_leak_globs_are_caught_with_context() {
        let wire = Tripwire::new(&["**/*rubric*".to_owned()]);
        assert!(wire.check_path("grader/rubric.md").is_err());
        assert!(wire.check_path("results/pmt-eval-verdict.json").is_err());
        assert!(wire.check_path("src/main.rs").is_ok());
        let err = check_text(
            "run.json",
            "{\n  \"suite\": \"smoke\",\n  \"pass_rate\": 0.5\n}",
        )
        .unwrap_err();
        assert!(
            err.contains("\\\"pass_rate\\\": 0.5") || err.contains("\"pass_rate\": 0.5"),
            "{err}"
        );
        assert!(check_text("a.md", "the score was good").is_ok());
        assert!(check_text("a.json", "{\"score\": \"high\"}").is_ok());
        let long = format!("{}{VERDICT_CANARY}{}", "x".repeat(100), "y".repeat(300));
        let err = check_text("v.txt", &long).unwrap_err();
        let quoted = err.split('"').nth(1).unwrap();
        assert_eq!(quoted.len(), BEFORE + AFTER, "{quoted}");
    }
}

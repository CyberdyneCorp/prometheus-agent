//! Scripts: parsed, never run; and every script the harness text names
//! must be in the tree.

use std::path::Path;
use std::sync::LazyLock;

use pmt_config::harness::Tree;
use pmt_tools::files::lint::{LintStatus, Linter, tree_sitter_check};
use regex::Regex;

use crate::harness::surface;

/// Parses every `.py` (Python grammar) and `.sh` (shell grammar) script.
pub(super) fn syntax(tree: &Tree, errors: &mut Vec<String>) {
    let linter = Linter::builtin();
    for (path, text) in tree.files.iter().filter(|(p, _)| surface::is_script(p)) {
        let verdict = if Path::new(path).extension().is_some_and(|e| e == "sh") {
            tree_sitter_check(text, &tree_sitter_bash::LANGUAGE.into())
        } else {
            python(&linter, path, text)
        };
        if let Err(message) = verdict {
            errors.push(format!("{path}: {message}"));
        }
    }
}

fn python(linter: &Linter, path: &str, text: &str) -> Result<(), String> {
    let lint = linter.check(Path::new(path), text);
    match lint.status {
        LintStatus::Ok => Ok(()),
        LintStatus::Error => Err(lint.output),
        LintStatus::Skipped => Err(format!("could not be checked ({})", lint.output)),
    }
}

/// A `scripts/<file>` or `skills/<name>/scripts/<file>` reference: not
/// preceded by a path character, so `x/scripts/a.py` is not one.
static REFERENCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?:^|[^A-Za-z0-9_./-])((?:skills/[a-z0-9][a-z0-9_-]{0,63}/)?scripts/[A-Za-z0-9_][A-Za-z0-9_.-]*\.(?:py|sh))\b",
    )
    .expect("script reference pattern")
});

/// Every script named in the harness's Markdown exists in the tree.
pub(super) fn references(tree: &Tree, errors: &mut Vec<String>) {
    for (path, text) in tree
        .files
        .iter()
        .filter(|(p, _)| Path::new(p).extension().is_some_and(|e| e == "md"))
    {
        let mut missing: Vec<&str> = REFERENCE
            .captures_iter(text)
            .filter_map(|c| c.get(1))
            .map(|m| m.as_str())
            .filter(|named| !tree.files.contains_key(*named))
            .collect();
        missing.sort_unstable();
        missing.dedup();
        for named in missing {
            errors.push(format!("{path} names {named}, which is not in the harness"));
        }
    }
}

/// The harness scripts `words` of a hook command name.
pub(super) fn named_in(words: &[String]) -> impl Iterator<Item = &str> {
    words
        .iter()
        .map(|w| w.trim_start_matches("./"))
        .filter(|w| w.contains('/') || pmt_config::harness::is_script(w))
}

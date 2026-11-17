//! The difference between two harness trees (spec: harness-evolution,
//! Harness history and diff): a unified diff plus a summary of added,
//! removed and modified paths and the change in always-on characters.

use std::fmt;

use pmt_config::harness::Tree;
use serde::Serialize;

/// What changed from one tree to another.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Summary {
    /// Paths only in the second tree.
    pub added: Vec<String>,
    /// Paths only in the first tree.
    pub removed: Vec<String>,
    /// Paths in both whose text differs.
    pub modified: Vec<String>,
    /// Always-on characters of the second tree minus the first's.
    pub always_on_char_delta: i64,
}

impl Summary {
    /// Whether the trees are identical.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.modified.is_empty()
    }
}

impl fmt::Display for Summary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "added: [{}], removed: [{}], modified: [{}], always_on_char_delta: {}",
            self.added.join(", "),
            self.removed.join(", "),
            self.modified.join(", "),
            self.always_on_char_delta
        )
    }
}

/// The summary of `from` → `to`.
#[must_use]
pub fn summary(from: &Tree, to: &Tree) -> Summary {
    let mut out = Summary::default();
    for (path, text) in &to.files {
        match from.get(path) {
            None => out.added.push(path.clone()),
            Some(old) if old != text => out.modified.push(path.clone()),
            Some(_) => {}
        }
    }
    out.removed = from
        .files
        .keys()
        .filter(|p| !to.files.contains_key(*p))
        .cloned()
        .collect();
    out.always_on_char_delta = signed(to.always_on_chars()) - signed(from.always_on_chars());
    out
}

fn signed(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// The unified diff of `from` → `to`, file by file in path order, with
/// `a/<label>/<path>` and `b/<label>/<path>` headers.
#[must_use]
pub fn unified(from: &Tree, to: &Tree, labels: (&str, &str)) -> String {
    let mut paths: Vec<&String> = from.files.keys().chain(to.files.keys()).collect();
    paths.sort();
    paths.dedup();
    let mut text = String::new();
    for path in paths {
        let old = from.get(path).unwrap_or_default();
        let new = to.get(path).unwrap_or_default();
        if old == new && from.files.contains_key(path) == to.files.contains_key(path) {
            continue;
        }
        let diff = similar::TextDiff::from_lines(old, new);
        text.push_str(
            &diff
                .unified_diff()
                .header(
                    &format!("a/{}/{path}", labels.0),
                    &format!("b/{}/{path}", labels.1),
                )
                .to_string(),
        );
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scenario: Diff summary.
    #[test]
    fn a_new_skill_and_a_grown_guardrails_file_summarize_as_the_spec_shows() {
        let h4 = Tree::builtin();
        let mut h5 = h4.clone();
        h5.files.insert(
            "skills/recon/SKILL.md".into(),
            "---\nname: recon\ndescription: reconciling ledgers\n---\n".into(),
        );
        h5.files.insert("GUARDRAILS.md".into(), "x".repeat(52));
        assert_eq!(
            summary(&h4, &h5).to_string(),
            "added: [skills/recon/SKILL.md], removed: [], modified: [GUARDRAILS.md], always_on_char_delta: 52"
        );
        let diff = unified(&h4, &h5, ("h4", "h5"));
        assert!(diff.contains("+++ b/h5/skills/recon/SKILL.md"), "{diff}");
        assert!(summary(&h5, &h5).is_empty());
    }
}

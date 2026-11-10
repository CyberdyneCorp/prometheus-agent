//! The summary quality check (spec: context-compression, Summary quality
//! check).
//!
//! The items a summary must not lose are extracted deterministically from
//! the summarized region (files written or patched, the latest user
//! request, tool errors never followed by a success of the same tool) and
//! from the `todo` list (its open items). Coverage is the share of them the
//! summary mentions; a summary below `compression.quality_min_coverage` is
//! asked for once more with the missing items listed, and whatever is
//! still missing is appended verbatim under `## Preserved References`.

use super::items::Items;
use super::summary::Sections;

/// The section that carries what the summary left out.
pub const PRESERVED: &str = "Preserved References";

/// What a summary is checked against.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Checklist {
    items: Vec<String>,
}

impl Checklist {
    /// The items of a region plus the open todo items (their text).
    #[must_use]
    pub fn new(region: &Items, open_todos: &[String]) -> Self {
        let mut items: Vec<String> = Vec::new();
        let candidates = region
            .files
            .iter()
            .chain(open_todos)
            .chain(&region.latest_request)
            .chain(&region.errors);
        for item in candidates {
            let item = item.trim();
            if !item.is_empty() && !items.iter().any(|i| i == item) {
                items.push(item.to_owned());
            }
        }
        Self { items }
    }

    /// The items `summary` does not mention, in checklist order.
    #[must_use]
    pub fn missing(&self, summary: &str) -> Vec<String> {
        let haystack = normalize(summary);
        self.items
            .iter()
            .filter(|item| !haystack.contains(&normalize(item)))
            .cloned()
            .collect()
    }

    /// The share of the items `summary` mentions (1 for no items).
    #[must_use]
    pub fn coverage(&self, summary: &str) -> f64 {
        if self.items.is_empty() {
            return 1.0;
        }
        let missing = self.missing(summary).len();
        let found = self.items.len() - missing;
        ratio(found, self.items.len())
    }
}

#[allow(clippy::cast_precision_loss)] // item counts are tiny
fn ratio(part: usize, whole: usize) -> f64 {
    part as f64 / whole as f64
}

/// Whitespace collapsed, so a wrapped line still matches.
fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `sections` with every item `document` (the rendered summary) misses
/// appended verbatim under [`PRESERVED`].
pub fn preserve(sections: &mut Sections, missing: &[String]) {
    let lines: Vec<String> = missing.iter().map(|m| format!("- {m}")).collect();
    sections.append(PRESERVED, &lines.join("\n"));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checklist() -> Checklist {
        let region = Items {
            files: vec!["src/auth.rs".into(), "src/db.rs".into()],
            latest_request: Some("fix the login".into()),
            errors: vec!["terminal: boom".into()],
        };
        Checklist::new(&region, &["write   docs".into()])
    }

    #[test]
    fn coverage_is_the_share_of_items_the_summary_mentions() {
        let list = checklist();
        let summary = "Patched src/db.rs to fix the login; still need to write\ndocs.";
        assert_eq!(list.missing(summary), ["src/auth.rs", "terminal: boom"]);
        assert!((list.coverage(summary) - 0.6).abs() < 1e-9);
        assert!((Checklist::default().coverage("") - 1.0).abs() < 1e-9);
    }

    #[test]
    fn missing_items_are_preserved_verbatim() {
        let mut sections = Sections::parse("## Goal\nShip.");
        preserve(&mut sections, &["src/auth.rs".into()]);
        let doc = sections.document(None, None);
        assert!(doc.contains("## Preserved References\n- src/auth.rs"));
    }
}

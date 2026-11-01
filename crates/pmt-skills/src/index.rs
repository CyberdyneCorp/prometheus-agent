//! The skills index of the system prompt (spec: skills, Skills index in
//! the system prompt).
//!
//! One `- <name>: <description>` row per visible skill, grouped by
//! category, descriptions clipped at a word boundary so the index stays
//! small however many skills a profile has. A posture can demote
//! categories to a names-only line (still discoverable, cheaper), and a
//! safe-worker session gets no index at all.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use crate::catalog::Skill;

/// The section heading.
pub const HEADING: &str = "## Skills";

/// Categories the coding posture shows by name only: work unrelated to
/// software keeps its skills discoverable without spending prompt on them.
pub const CODING_DEMOTED: &[&str] = &["creative", "gaming", "leisure", "media", "social-media"];

/// How the session renders its index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Posture {
    /// Every category in full.
    #[default]
    Default,
    /// [`CODING_DEMOTED`] names only.
    Coding,
    /// No index (safe-worker sessions).
    SafeWorker,
}

impl Posture {
    fn demoted(self, category: &str) -> bool {
        self == Self::Coding && CODING_DEMOTED.contains(&category)
    }
}

/// `text` clipped to at most `max` characters, ending at a word boundary
/// (trailing punctuation and spaces trimmed when clipped).
#[must_use]
pub fn clip(text: &str, max: usize) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() <= max {
        return text;
    }
    let cut: String = text.chars().take(max + 1).collect();
    let end = cut.rfind(' ').unwrap_or(0);
    let clipped = if end == 0 {
        text.chars().take(max).collect::<String>()
    } else {
        cut[..end].to_owned()
    };
    clipped
        .trim_end_matches(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | ':' | '-'))
        .to_owned()
}

/// The index section of `skills` (already gated), `None` when empty.
#[must_use]
pub fn render(skills: &[&Skill], description_chars: usize, posture: Posture) -> Option<String> {
    if posture == Posture::SafeWorker || skills.is_empty() {
        return None;
    }
    let mut groups: BTreeMap<&str, Vec<&Skill>> = BTreeMap::new();
    for skill in skills {
        groups
            .entry(skill.category.as_str())
            .or_default()
            .push(skill);
    }
    let mut text = format!(
        "{HEADING}\nBefore answering, check whether a skill below fits the task; if one does, \
         load it with skill_view(name) and follow it.\n<available_skills>\n"
    );
    for (category, mut members) in groups {
        members.sort_by(|a, b| a.name.cmp(&b.name));
        if posture.demoted(category) {
            let names: Vec<&str> = members.iter().map(|s| s.name.as_str()).collect();
            let _ = writeln!(text, "{category} [names only]: {}", names.join(", "));
            continue;
        }
        let _ = writeln!(text, "{category}:");
        for skill in members {
            let _ = writeln!(
                text,
                "- {}: {}",
                skill.name,
                clip(&skill.description, description_chars)
            );
        }
    }
    text.push_str("</available_skills>");
    Some(text)
}

/// What an index showed: name -> description.
pub type Snapshot = BTreeMap<String, String>;

/// The snapshot of `skills`.
#[must_use]
pub fn snapshot(skills: &[&Skill]) -> Snapshot {
    skills
        .iter()
        .map(|s| (s.name.clone(), s.description.clone()))
        .collect()
}

/// How the current skills differ from those a prompt showed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Difference {
    /// New names.
    pub added: BTreeSet<String>,
    /// Names gone.
    pub removed: BTreeSet<String>,
    /// Same name, new description.
    pub changed: BTreeSet<String>,
}

impl Difference {
    /// `current` against `frozen`.
    #[must_use]
    pub fn between(frozen: &Snapshot, current: &Snapshot) -> Self {
        let mut diff = Self::default();
        for (name, description) in current {
            match frozen.get(name) {
                None => {
                    diff.added.insert(name.clone());
                }
                Some(old) if old != description => {
                    diff.changed.insert(name.clone());
                }
                Some(_) => {}
            }
        }
        diff.removed = frozen
            .keys()
            .filter(|n| !current.contains_key(*n))
            .cloned()
            .collect();
        diff
    }

    /// Whether nothing differs.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }
}

/// The note prefix (spec text).
pub const NOTE_PREFIX: &str =
    "[System: The skills available in this conversation changed after its system prompt was built.";

/// The per-turn note stating the full `diff` (`current` gives the new
/// descriptions).
#[must_use]
pub fn note(diff: &Difference, current: &Snapshot, description_chars: usize) -> String {
    if diff.is_empty() {
        return format!("{NOTE_PREFIX} They match the system prompt again.]");
    }
    let mut text = NOTE_PREFIX.to_owned();
    let row = |name: &String| {
        let description = current.get(name).map_or("", String::as_str);
        format!("{name}: {}", clip(description, description_chars))
    };
    if !diff.added.is_empty() {
        let rows: Vec<String> = diff.added.iter().map(row).collect();
        let _ = write!(text, " Added: {}.", rows.join("; "));
    }
    if !diff.changed.is_empty() {
        let rows: Vec<String> = diff.changed.iter().map(row).collect();
        let _ = write!(text, " Changed: {}.", rows.join("; "));
    }
    if !diff.removed.is_empty() {
        let names: Vec<&str> = diff.removed.iter().map(String::as_str).collect();
        let _ = write!(text, " Removed: {}.", names.join(", "));
    }
    text.push_str(" Use skills_list() for the current set.]");
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipping_ends_at_a_word_boundary() {
        let long = "Analyze large CSV exports, join them with the warehouse tables and produce weekly revenue reports for finance with charts.";
        let clipped = clip(long, 60);
        assert!(clipped.chars().count() <= 60, "{clipped}");
        assert!(long.starts_with(&clipped));
        let next = long[clipped.len()..].chars().next();
        assert!(next.is_some_and(|c| c == ' ' || c == ','), "{clipped:?}");
        assert_eq!(clip("Short one.", 60), "Short one.");
    }

    #[test]
    fn differences_name_every_change() {
        let frozen: Snapshot = [("a".into(), "A".into()), ("b".into(), "B".into())].into();
        let current: Snapshot = [("a".into(), "A2".into()), ("c".into(), "C".into())].into();
        let diff = Difference::between(&frozen, &current);
        let text = note(&diff, &current, 60);
        assert!(text.starts_with(NOTE_PREFIX));
        assert!(text.contains("Added: c: C."), "{text}");
        assert!(text.contains("Changed: a: A2."), "{text}");
        assert!(text.contains("Removed: b."), "{text}");
    }
}

//! Distilled lessons (spec: experience-library, Experience distillation).
//! The reflector replies with an `<edits>` block — per changed component
//! the lines `component:`, `candidates:`, `change:`, `observed:` and
//! `verdict:` — then either an `<experience>` block with single-line
//! `cue:`, `lesson:`, `evidence:` and `confidence:` or `NO_ENTRY`. A reply
//! without a cue, lesson or evidence carries no lesson.

use serde::{Deserialize, Serialize};

/// How sure the reflector is of a lesson.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Confidence {
    /// Weakly supported: never offered to sessions.
    Low,
    /// Supported by the trajectories.
    Medium,
    /// Clearly supported by several rollouts.
    High,
}

impl Confidence {
    /// The spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    fn parse(word: &str) -> Option<Self> {
        match word.to_ascii_lowercase().as_str() {
            "high" => Some(Self::High),
            "medium" => Some(Self::Medium),
            "low" => Some(Self::Low),
            _ => None,
        }
    }
}

/// One transferable lesson.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lesson {
    /// When the lesson applies.
    pub cue: String,
    /// What to do.
    pub lesson: String,
    /// The trajectory moments behind it.
    pub evidence: String,
    /// How sure the reflector is.
    pub confidence: Confidence,
    /// Why.
    #[serde(default)]
    pub confidence_reason: String,
}

impl Lesson {
    /// `high (two rollouts agree)`, or the bare level.
    #[must_use]
    pub fn confidence_line(&self) -> String {
        if self.confidence_reason.is_empty() {
            self.confidence.as_str().to_owned()
        } else {
            format!("{} ({})", self.confidence.as_str(), self.confidence_reason)
        }
    }
}

/// One row of the `<edits>` block.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edit {
    /// The changed component.
    pub component: String,
    /// The candidates that carried it.
    pub candidates: String,
    /// What changed.
    pub change: String,
    /// What the rollouts showed.
    pub observed: String,
    /// `helped`, `hurt`, `no observed effect` or `confounded`.
    pub verdict: String,
}

/// What a reflector reply holds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Distilled {
    /// The edit rows.
    pub edits: Vec<Edit>,
    /// The lesson, if any.
    pub lesson: Option<Lesson>,
}

/// The text between `<tag>` and `</tag>`.
fn block<'a>(text: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let start = text.find(&open)? + open.len();
    let rest = &text[start..];
    let end = rest.find(&format!("</{tag}>")).unwrap_or(rest.len());
    Some(&rest[..end])
}

/// `key: value` of a line (keys lower-cased, a leading `-` dropped).
fn field(line: &str) -> Option<(String, String)> {
    let line = line.trim().trim_start_matches(['-', '*']).trim();
    let (key, value) = line.split_once(':')?;
    let key = key.trim().trim_matches('*').to_ascii_lowercase();
    Some((key, value.trim().to_owned()))
}

fn edits(text: &str) -> Vec<Edit> {
    let mut rows = Vec::new();
    let mut row = Edit::default();
    for (key, value) in text.lines().filter_map(field) {
        if key == "component" && !row.component.is_empty() {
            rows.push(std::mem::take(&mut row));
        }
        match key.as_str() {
            "component" => row.component = value,
            "candidates" => row.candidates = value,
            "change" => row.change = value,
            "observed" => row.observed = value,
            "verdict" => row.verdict = value,
            _ => {}
        }
    }
    if !row.component.is_empty() {
        rows.push(row);
    }
    rows
}

fn lesson(text: &str) -> Option<Lesson> {
    let mut found = Lesson {
        cue: String::new(),
        lesson: String::new(),
        evidence: String::new(),
        confidence: Confidence::Low,
        confidence_reason: String::new(),
    };
    for (key, value) in text.lines().filter_map(field) {
        match key.as_str() {
            "cue" => found.cue = value,
            "lesson" => found.lesson = value,
            "evidence" => found.evidence = value,
            "confidence" => {
                let (word, reason) = value
                    .split_once(char::is_whitespace)
                    .unwrap_or((&value, ""));
                if let Some(level) =
                    Confidence::parse(word.trim_matches(|c: char| !c.is_alphabetic()))
                {
                    found.confidence = level;
                    reason
                        .trim()
                        .trim_start_matches(['-', '—', ':', '(', ','])
                        .trim_end_matches(')')
                        .trim()
                        .clone_into(&mut found.confidence_reason);
                }
            }
            _ => {}
        }
    }
    let complete = [&found.cue, &found.lesson, &found.evidence]
        .iter()
        .all(|v| !v.is_empty());
    complete.then_some(found)
}

/// Parses a reflector reply.
#[must_use]
pub fn parse(reply: &str) -> Distilled {
    Distilled {
        edits: block(reply, "edits").map(edits).unwrap_or_default(),
        lesson: block(reply, "experience").and_then(lesson),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPLY: &str = "<edits>\ncomponent: GUARDRAILS.md\ncandidates: inv1, agg1\n\
        change: sum negative amounts too\nobserved: base1 skipped refunds\nverdict: helped\n\n\
        component: scripts/sum.py\ncandidates: agg1\nchange: parse signs\nobserved: one rollout\n\
        verdict: confounded\n</edits>\n<experience>\ncue: ledgers with refunds\n\
        lesson: sum signed amounts\nevidence: base1 step 4 dropped -3.20\n\
        confidence: medium — two rollouts agree\n</experience>";

    #[test]
    fn a_full_reply_carries_its_edits_and_lesson() {
        let distilled = parse(REPLY);
        assert_eq!(distilled.edits.len(), 2);
        assert_eq!(distilled.edits[1].verdict, "confounded");
        let lesson = distilled.lesson.unwrap();
        assert_eq!(lesson.cue, "ledgers with refunds");
        assert_eq!(lesson.confidence, Confidence::Medium);
        assert_eq!(lesson.confidence_reason, "two rollouts agree");
    }

    /// Scenario: All edits confounded.
    #[test]
    fn no_entry_and_incomplete_lessons_yield_none() {
        let none = parse("<edits>\ncomponent: SYSTEM.md\nverdict: confounded\n</edits>\nNO_ENTRY");
        assert_eq!(none.lesson, None);
        assert_eq!(none.edits.len(), 1);
        let partial = parse("<experience>\ncue: x\nlesson: y\n</experience>");
        assert_eq!(partial.lesson, None, "evidence is required");
    }
}

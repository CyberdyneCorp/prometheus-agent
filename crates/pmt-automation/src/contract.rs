//! Completion contracts (spec: session-automation, Completion contracts):
//! what "done" means for a goal. A contract comes from inline lines of the
//! goal text (`verify: …`) or from `/goal draft`, which asks the judge to
//! expand an objective.

use serde::{Deserialize, Serialize};

/// A goal's completion contract.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Contract {
    /// What the finished state looks like.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    /// The concrete evidence `done` needs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification: Option<String>,
    /// What must hold throughout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub constraints: Option<String>,
    /// What is out of scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boundaries: Option<String>,
    /// When to stop and report instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_when: Option<String>,
}

/// The inline prefixes and the field each fills.
const PREFIXES: &[(&str, Field)] = &[
    ("verify:", Field::Verification),
    ("verified by:", Field::Verification),
    ("constraints:", Field::Constraints),
    ("preserve:", Field::Constraints),
    ("boundaries:", Field::Boundaries),
    ("scope:", Field::Boundaries),
    ("stop when:", Field::StopWhen),
    ("blocked:", Field::StopWhen),
];

#[derive(Debug, Clone, Copy)]
enum Field {
    Verification,
    Constraints,
    Boundaries,
    StopWhen,
}

impl Contract {
    fn slot(&mut self, field: Field) -> &mut Option<String> {
        match field {
            Field::Verification => &mut self.verification,
            Field::Constraints => &mut self.constraints,
            Field::Boundaries => &mut self.boundaries,
            Field::StopWhen => &mut self.stop_when,
        }
    }

    /// Whether every field is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fields().is_empty()
    }

    /// The set fields as `(label, value)`, in contract order.
    #[must_use]
    pub fn fields(&self) -> Vec<(&'static str, &str)> {
        [
            ("Outcome", &self.outcome),
            ("Verification", &self.verification),
            ("Constraints", &self.constraints),
            ("Boundaries", &self.boundaries),
            ("Stop when", &self.stop_when),
        ]
        .into_iter()
        .filter_map(|(label, value)| value.as_deref().map(|v| (label, v)))
        .collect()
    }

    /// The contract as `/goal show` prints it and the judge reads it.
    #[must_use]
    pub fn render(&self) -> String {
        self.fields()
            .iter()
            .map(|(label, value)| format!("{label}: {value}"))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Splits a goal text into its headline and inline contract: a line whose
/// prefix (case-insensitive) is one of the contract prefixes fills that
/// field; every other line, including lines with incidental colons, stays
/// in the headline.
#[must_use]
pub fn parse_inline(text: &str) -> (String, Option<Contract>) {
    let mut contract = Contract::default();
    let mut headline = Vec::new();
    for line in text.lines() {
        match field_of(line) {
            Some((field, value)) => append(contract.slot(field), value),
            None => headline.push(line),
        }
    }
    let headline = headline.join("\n").trim().to_owned();
    (headline, (!contract.is_empty()).then_some(contract))
}

fn field_of(line: &str) -> Option<(Field, &str)> {
    let trimmed = line.trim_start();
    let lower = trimmed.to_lowercase();
    PREFIXES.iter().find_map(|(prefix, field)| {
        lower
            .starts_with(prefix)
            .then(|| (*field, trimmed[prefix.len()..].trim()))
    })
}

fn append(slot: &mut Option<String>, value: &str) {
    if value.is_empty() {
        return;
    }
    match slot {
        Some(existing) => {
            existing.push('\n');
            existing.push_str(value);
        }
        None => *slot = Some(value.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_lines_fill_the_contract_and_the_rest_stays_headline() {
        let (headline, contract) =
            parse_inline("Fix bug: parser drops commas\nverify: cargo test -p parser passes");
        assert_eq!(headline, "Fix bug: parser drops commas");
        let contract = contract.unwrap();
        assert_eq!(
            contract.verification.as_deref(),
            Some("cargo test -p parser passes")
        );
        assert!(contract.outcome.is_none());
    }

    #[test]
    fn every_prefix_maps_to_its_field() {
        let (headline, contract) = parse_inline(
            "Migrate\nVerified by: ci green\npreserve: public API\nscope: auth only\nblocked: needs creds",
        );
        assert_eq!(headline, "Migrate");
        let contract = contract.unwrap();
        assert_eq!(contract.verification.as_deref(), Some("ci green"));
        assert_eq!(contract.constraints.as_deref(), Some("public API"));
        assert_eq!(contract.boundaries.as_deref(), Some("auth only"));
        assert_eq!(contract.stop_when.as_deref(), Some("needs creds"));
        assert!(contract.render().starts_with("Verification: ci green"));
    }

    #[test]
    fn a_goal_without_prefixed_lines_has_no_contract() {
        let (headline, contract) = parse_inline("note: keep going\nthen stop");
        assert_eq!(headline, "note: keep going\nthen stop");
        assert!(contract.is_none());
    }
}

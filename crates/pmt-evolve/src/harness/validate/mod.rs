//! Harness validation (spec: harness-evolution, Harness validation;
//! Harness size caps; Harness execution confinement).
//!
//! Every rule runs and every failure is collected, so one pass tells the
//! author (a person or an improver session) everything that is wrong.
//! Validation also performs the one repair the spec allows: a skill that
//! no always-on file names gets a trigger line appended to
//! `GUARDRAILS.md`, so a valid harness never carries an unreachable skill.

pub mod caps;
pub mod hooks;
mod scripts;
pub mod skills;

use std::collections::{BTreeMap, BTreeSet};

use pmt_config::harness::{Census, Tree};
use pmt_core::secret::Redactor;

use super::surface;

/// What a tree is checked against.
#[derive(Debug, Clone)]
pub struct Rules {
    /// `harness.always_on_cap_chars`.
    pub always_on_cap: usize,
    /// Names of the skills installed in the profile (a harness skill may
    /// not shadow one).
    pub installed_skills: BTreeSet<String>,
    /// Toolsets a subagent definition may name.
    pub toolsets: BTreeSet<String>,
    /// The profile's known secrets (plus credential-shaped tokens).
    pub redactor: Redactor,
}

impl Rules {
    /// Rules with the default cap, no installed skills, the given
    /// toolsets and no known secrets (credential shapes still match).
    #[must_use]
    pub fn with_toolsets(toolsets: impl IntoIterator<Item = String>) -> Self {
        Self {
            always_on_cap: 24_000,
            installed_skills: BTreeSet::new(),
            toolsets: toolsets.into_iter().collect(),
            redactor: Redactor::default(),
        }
    }
}

/// A tree that passed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Validated {
    /// What it holds.
    pub census: Census,
    /// Skills that were given a trigger line in `GUARDRAILS.md`.
    pub repaired: Vec<String>,
}

/// Validates `tree`, normalizing its keys and repairing skill trigger
/// lines in place.
///
/// # Errors
///
/// Every rule the tree breaks, one message each.
pub fn validate(tree: &mut Tree, rules: &Rules) -> Result<Validated, Vec<String>> {
    let mut errors = Vec::new();
    tree.files = normalized(std::mem::take(&mut tree.files), &mut errors);
    caps::files(tree, &mut errors);
    let valid_skills = skills::check(tree, rules, &mut errors);
    let repaired = skills::repair_triggers(tree, &valid_skills, rules.always_on_cap, &mut errors);
    skills::agents(tree, rules, &mut errors);
    scripts::syntax(tree, &mut errors);
    scripts::references(tree, &mut errors);
    hooks::check(tree, &mut errors);
    text_rules(tree, rules, &mut errors);
    caps::always_on(tree, rules.always_on_cap, &mut errors);
    if errors.is_empty() {
        Ok(Validated {
            census: Census::of(tree),
            repaired,
        })
    } else {
        Err(errors)
    }
}

/// The tree with normalized keys; bad paths and off-surface paths are
/// reported and dropped from the rest of the checks.
fn normalized(
    files: BTreeMap<String, String>,
    errors: &mut Vec<String>,
) -> BTreeMap<String, String> {
    let mut kept = BTreeMap::new();
    let mut off_surface = Vec::new();
    for (key, text) in files {
        match pmt_config::harness::normalize(&key) {
            Err(why) => errors.push(format!("invalid path '{key}': {why}")),
            Ok(path) if surface::classify(&path).is_none() => off_surface.push(path),
            Ok(path) => {
                kept.insert(path, text);
            }
        }
    }
    if !off_surface.is_empty() {
        errors.push(format!(
            "off-surface paths: {}",
            surface::listing(&off_surface)
        ));
    }
    kept
}

/// No template syntax in always-on text; no secret anywhere.
fn text_rules(tree: &Tree, rules: &Rules, errors: &mut Vec<String>) {
    for path in pmt_config::harness::ALWAYS_ON {
        if tree
            .get(path)
            .is_some_and(|t| t.contains("{{") || t.contains("{%"))
        {
            errors.push(format!("{path} contains template syntax ('{{{{' or '{{%')"));
        }
    }
    for (path, text) in &tree.files {
        if rules.redactor.redact_all(text) != *text {
            errors.push(format!("{path} contains a secret"));
        }
    }
}

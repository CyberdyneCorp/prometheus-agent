//! Skills and subagent definitions: front matter, name collisions and the
//! auto-trigger repair.

use std::collections::BTreeSet;

use pmt_config::harness::{ALWAYS_ON, Tree};
use serde_json::Value;

use super::Rules;

/// Longest description of a harness skill or subagent.
pub const MAX_DESCRIPTION_CHARS: usize = 600;

/// A skill whose `SKILL.md` passed.
pub(super) struct Skill {
    name: String,
    description: String,
}

/// Checks every `skills/<name>/SKILL.md` and returns the valid ones.
pub(super) fn check(tree: &Tree, rules: &Rules, errors: &mut Vec<String>) -> Vec<Skill> {
    let mut valid = Vec::new();
    let mut skills = BTreeSet::new();
    for (path, text) in &tree.files {
        let Some(dir) = path
            .strip_prefix("skills/")
            .and_then(|rest| rest.strip_suffix("/SKILL.md"))
        else {
            continue;
        };
        skills.insert(dir.to_owned());
        if rules.installed_skills.contains(dir) {
            errors.push(format!(
                "skill '{dir}' is already installed in the profile; a harness skill may not shadow it"
            ));
        }
        match front_matter(path, dir, text) {
            Ok((_, description)) => valid.push(Skill {
                name: dir.to_owned(),
                description,
            }),
            Err(error) => errors.push(error),
        }
    }
    for path in tree.files.keys() {
        if let Some((dir, _)) = path
            .strip_prefix("skills/")
            .and_then(|rest| rest.split_once("/scripts/"))
            && !skills.contains(dir)
        {
            errors.push(format!(
                "{path} belongs to no skill (skills/{dir}/SKILL.md is missing)"
            ));
        }
    }
    valid
}

/// `name` and `description` of a definition at `path` whose directory is
/// `dir`.
fn front_matter(
    path: &str,
    dir: &str,
    text: &str,
) -> Result<(serde_json::Map<String, Value>, String), String> {
    let doc = pmt_skills::frontmatter::parse(text)
        .map_err(|e| format!("{path}: line {}: {}", e.line, e.reason))?;
    if doc.name != dir {
        return Err(format!(
            "{path}: name '{}' does not match its directory '{dir}'",
            doc.name
        ));
    }
    let description = doc.description.trim().to_owned();
    if description.is_empty() {
        return Err(format!("{path}: description is empty"));
    }
    if description.chars().count() > MAX_DESCRIPTION_CHARS {
        return Err(format!(
            "{path}: description is longer than {MAX_DESCRIPTION_CHARS} characters"
        ));
    }
    Ok((doc.front, description))
}

/// Whether always-on text names `skill` as a word.
fn named(tree: &Tree, skill: &str) -> bool {
    let word = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    ALWAYS_ON.iter().filter_map(|p| tree.get(p)).any(|text| {
        text.match_indices(skill).any(|(at, _)| {
            let before = text[..at].chars().next_back();
            let after = text[at + skill.len()..].chars().next();
            !before.is_some_and(word) && !after.is_some_and(word)
        })
    })
}

/// Appends ``- When <description>, load the `<name>` skill.`` to
/// `GUARDRAILS.md` for every skill no always-on file names, skipping a
/// line that would break the always-on cap (that skill is then an error).
pub(super) fn repair_triggers(
    tree: &mut Tree,
    skills: &[Skill],
    cap: usize,
    errors: &mut Vec<String>,
) -> Vec<String> {
    let mut repaired = Vec::new();
    for skill in skills {
        if named(tree, &skill.name) {
            continue;
        }
        let description = skill.description.trim_end_matches('.');
        let line = format!("- When {description}, load the `{}` skill.\n", skill.name);
        let guardrails = tree.files.entry("GUARDRAILS.md".to_owned()).or_default();
        let separator = if guardrails.is_empty() || guardrails.ends_with('\n') {
            ""
        } else {
            "\n"
        };
        let added = separator.chars().count() + line.chars().count();
        if tree.always_on_chars() + added > cap {
            errors.push(format!(
                "skill '{}' is not named in always-on text, and its trigger line would exceed the always-on cap",
                skill.name
            ));
            continue;
        }
        let guardrails = tree.files.entry("GUARDRAILS.md".to_owned()).or_default();
        guardrails.push_str(separator);
        guardrails.push_str(&line);
        repaired.push(skill.name.clone());
    }
    repaired
}

/// Checks every `agents/<name>/AGENT.md`: front matter, and only known
/// toolsets.
pub(super) fn agents(tree: &Tree, rules: &Rules, errors: &mut Vec<String>) {
    for (path, text) in &tree.files {
        let Some(dir) = path
            .strip_prefix("agents/")
            .and_then(|rest| rest.strip_suffix("/AGENT.md"))
        else {
            continue;
        };
        let front = match front_matter(path, dir, text) {
            Ok((front, _)) => front,
            Err(error) => {
                errors.push(error);
                continue;
            }
        };
        match front.get("toolsets") {
            None | Some(Value::Null) => {}
            Some(Value::Array(items)) => {
                for item in items {
                    match item.as_str() {
                        Some(name) if rules.toolsets.contains(name) => {}
                        Some(name) => errors.push(format!("{path}: unknown toolset '{name}'")),
                        None => errors.push(format!("{path}: toolsets must list names")),
                    }
                }
            }
            Some(_) => errors.push(format!("{path}: toolsets must be a list")),
        }
    }
}

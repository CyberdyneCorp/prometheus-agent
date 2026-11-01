//! `SKILL.md`: YAML frontmatter between `---` lines, then a Markdown body
//! (spec: skills, SKILL.md format and agentskills.io compatibility).
//!
//! Only the agentskills.io fields `name` and `description` are required;
//! every other key is kept as written (the optional agentskills.io fields
//! `license`, `compatibility`, `allowed-tools` and `metadata` pass through
//! unchanged). Prometheus-specific fields are read with [`field`], which
//! looks under `metadata.prometheus`, then `metadata.hermes`, then the top
//! level, so Hermes skills load without edits.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value};

use crate::yaml;

/// Longest skill or category name.
pub const MAX_NAME_CHARS: usize = 64;
/// Longest description.
pub const MAX_DESCRIPTION_CHARS: usize = 1024;
/// Largest `SKILL.md`, in characters.
pub const MAX_SKILL_CHARS: usize = 100_000;

static NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z0-9][a-z0-9._-]*$").expect("name pattern"));

/// Whether `name` is a valid skill or category name.
#[must_use]
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.chars().count() <= MAX_NAME_CHARS && NAME.is_match(name)
}

/// A parsed `SKILL.md`.
#[derive(Debug, Clone, PartialEq)]
pub struct Document {
    /// The frontmatter mapping, as written.
    pub front: Map<String, Value>,
    /// `name`.
    pub name: String,
    /// `description`.
    pub description: String,
    /// The Markdown after the closing `---` line.
    pub body: String,
}

/// Why a `SKILL.md` was not loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invalid {
    /// 1-based line in the file.
    pub line: usize,
    /// What is wrong.
    pub reason: String,
}

fn invalid(line: usize, reason: impl Into<String>) -> Invalid {
    Invalid {
        line,
        reason: reason.into(),
    }
}

/// Splits `text` into frontmatter text (from line 2) and body.
fn split(text: &str) -> Result<(&str, &str), Invalid> {
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);
    let mut lines = text.split_inclusive('\n');
    let first = lines.next().unwrap_or_default();
    if first.trim_end() != "---" {
        return Err(invalid(1, "SKILL.md must start with a '---' line"));
    }
    let start = first.len();
    let mut offset = start;
    for line in lines {
        if line.trim_end() == "---" {
            return Ok((&text[start..offset], &text[offset + line.len()..]));
        }
        offset += line.len();
    }
    Err(invalid(1, "invalid frontmatter: no closing '---' line"))
}

/// Parses a `SKILL.md`.
///
/// # Errors
///
/// [`Invalid`] with the line and reason: no frontmatter, YAML that does
/// not parse (`invalid frontmatter: ...`), a missing or malformed `name`
/// or `description`.
pub fn parse(text: &str) -> Result<Document, Invalid> {
    let (yaml_text, body) = split(text)?;
    let value = yaml::parse(yaml_text)
        .map_err(|e| invalid(e.line + 1, format!("invalid frontmatter: {}", e.reason)))?;
    let Value::Object(front) = value else {
        return Err(invalid(2, "invalid frontmatter: not a mapping"));
    };
    let name = required(&front, "name")?;
    if !valid_name(&name) {
        return Err(invalid(
            line_of(yaml_text, "name"),
            format!(
                "invalid name '{name}': 1-{MAX_NAME_CHARS} characters matching ^[a-z0-9][a-z0-9._-]*$"
            ),
        ));
    }
    let description = required(&front, "description")?;
    if description.chars().count() > MAX_DESCRIPTION_CHARS {
        return Err(invalid(
            line_of(yaml_text, "description"),
            format!("description is longer than {MAX_DESCRIPTION_CHARS} characters"),
        ));
    }
    Ok(Document {
        front,
        name,
        description,
        body: body.to_owned(),
    })
}

fn required(front: &Map<String, Value>, key: &str) -> Result<String, Invalid> {
    match front.get(key) {
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(s.trim().to_owned()),
        Some(_) => Err(invalid(
            1,
            format!("field '{key}' must be a non-empty string"),
        )),
        None => Err(invalid(1, format!("missing required field '{key}'"))),
    }
}

/// The file line of top-level `key` (frontmatter starts on line 2).
fn line_of(yaml_text: &str, key: &str) -> usize {
    let prefix = format!("{key}:");
    yaml_text
        .lines()
        .position(|l| l.starts_with(&prefix))
        .map_or(1, |i| i + 2)
}

/// A Prometheus-specific field: `metadata.prometheus.<key>`, else
/// `metadata.hermes.<key>`, else the top-level `<key>`.
#[must_use]
pub fn field<'a>(front: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    let metadata = front.get("metadata").and_then(Value::as_object);
    ["prometheus", "hermes"]
        .iter()
        .find_map(|ns| metadata?.get(*ns)?.as_object()?.get(key))
        .or_else(|| front.get(key))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn hermes_fields_are_found_under_metadata() {
        let doc = parse(
            "\u{FEFF}---\nname: axolotl\ndescription: Fine-tune LLMs with Axolotl.\n\
             license: MIT\nmetadata:\n  hermes:\n    tags: [mlops]\n---\n# Axolotl\n",
        )
        .unwrap();
        assert_eq!(doc.name, "axolotl");
        assert_eq!(field(&doc.front, "tags"), Some(&json!(["mlops"])));
        assert_eq!(doc.front.get("license"), Some(&json!("MIT")));
        assert_eq!(doc.body, "# Axolotl\n");
    }

    #[test]
    fn prometheus_namespace_wins_then_top_level() {
        let doc = parse(
            "---\nname: a\ndescription: d\ntags: [top]\ncategory: c\nmetadata:\n  prometheus:\n    tags: [p]\n  hermes:\n    tags: [h]\n---\n",
        )
        .unwrap();
        assert_eq!(field(&doc.front, "tags"), Some(&json!(["p"])));
        assert_eq!(field(&doc.front, "category"), Some(&json!("c")));
    }

    #[test]
    fn problems_name_their_line() {
        let err = parse("---\nname: x\ndescription: y\ntags: [a, b\n---\nbody\n").unwrap_err();
        assert!(err.reason.starts_with("invalid frontmatter"), "{err:?}");
        assert!(err.line >= 4, "{err:?}");
        assert_eq!(parse("# no frontmatter\n").unwrap_err().line, 1);
        let err = parse("---\ndescription: y\n---\n").unwrap_err();
        assert_eq!(err.reason, "missing required field 'name'");
        let err = parse("---\nname: Bad Name\ndescription: y\n---\n").unwrap_err();
        assert_eq!(err.line, 2);
    }
}

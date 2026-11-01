//! The Prometheus-specific frontmatter fields, typed (spec: skills,
//! SKILL.md format; Platform, environment and conditional gates; Skill
//! secrets and settings).
//!
//! Each field is read with [`crate::frontmatter::field`]. A value of the
//! wrong shape is ignored rather than refusing the skill: these fields only
//! narrow where a skill shows up, and a typo must not hide a whole skill.

use serde_json::{Map, Value};

use crate::frontmatter::field;

/// A setting the skill reads (`skills.config.<skill>.<key>`).
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigDecl {
    /// The key.
    pub key: String,
    /// What it means.
    pub description: Option<String>,
    /// Used when the profile sets nothing.
    pub default: Option<Value>,
}

/// A secret the skill needs (an environment variable name).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretDecl {
    /// The variable / secret name.
    pub name: String,
    /// What to tell the user when asking for it.
    pub prompt: Option<String>,
}

/// The typed fields.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Meta {
    /// `tags`.
    pub tags: Vec<String>,
    /// `category` (overrides the directory's).
    pub category: Option<String>,
    /// `related_skills`.
    pub related_skills: Vec<String>,
    /// `platforms` (`macos`, `linux`, `windows`, `android`; empty = all).
    pub platforms: Vec<String>,
    /// `requires_toolsets`.
    pub requires_toolsets: Vec<String>,
    /// `requires_tools`.
    pub requires_tools: Vec<String>,
    /// `fallback_for_toolsets`.
    pub fallback_for_toolsets: Vec<String>,
    /// `fallback_for_tools`.
    pub fallback_for_tools: Vec<String>,
    /// `session_platforms` (`None` = every surface).
    pub session_platforms: Option<Vec<String>>,
    /// `config`: non-secret settings.
    pub config: Vec<ConfigDecl>,
    /// `secrets` (alias `required_environment_variables`, legacy
    /// `prerequisites.env_vars`).
    pub secrets: Vec<SecretDecl>,
    /// Whether a `blueprint` block is present (scheduling is M2).
    pub blueprint: bool,
}

impl Meta {
    /// The fields of `front`.
    #[must_use]
    pub fn of(front: &Map<String, Value>) -> Self {
        let list = |key: &str| strings(field(front, key));
        Self {
            tags: list("tags"),
            category: field(front, "category")
                .and_then(Value::as_str)
                .map(str::to_owned),
            related_skills: list("related_skills"),
            platforms: list("platforms")
                .into_iter()
                .map(|p| p.to_ascii_lowercase())
                .collect(),
            requires_toolsets: list("requires_toolsets"),
            requires_tools: list("requires_tools"),
            fallback_for_toolsets: list("fallback_for_toolsets"),
            fallback_for_tools: list("fallback_for_tools"),
            session_platforms: field(front, "session_platforms")
                .map(|v| strings(Some(v)))
                .filter(|v| !v.is_empty()),
            config: config_decls(field(front, "config")),
            secrets: secret_decls(front),
            blueprint: field(front, "blueprint").is_some_and(|v| !v.is_null()),
        }
    }
}

/// A string or a list of strings.
fn strings(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

fn text(map: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|k| map.get(*k)?.as_str())
        .map(str::to_owned)
}

/// `[{key, description, default}]` or `{key: {description, default}}`.
fn config_decls(value: Option<&Value>) -> Vec<ConfigDecl> {
    let entry = |key: String, map: Option<&Map<String, Value>>| ConfigDecl {
        key,
        description: map.and_then(|m| text(m, &["description", "prompt"])),
        default: map.and_then(|m| m.get("default")).cloned(),
    };
    match value {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| match item {
                Value::String(key) => Some(entry(key.clone(), None)),
                Value::Object(map) => text(map, &["key", "name"]).map(|k| entry(k, Some(map))),
                _ => None,
            })
            .collect(),
        Some(Value::Object(map)) => map
            .iter()
            .map(|(key, v)| entry(key.clone(), v.as_object()))
            .collect(),
        _ => Vec::new(),
    }
}

fn secret_decls(front: &Map<String, Value>) -> Vec<SecretDecl> {
    let legacy = front
        .get("prerequisites")
        .and_then(Value::as_object)
        .and_then(|p| p.get("env_vars"));
    let value = field(front, "secrets")
        .or_else(|| field(front, "required_environment_variables"))
        .or(legacy);
    let Some(Value::Array(items)) = value else {
        return strings(value)
            .into_iter()
            .map(|name| SecretDecl { name, prompt: None })
            .collect();
    };
    items
        .iter()
        .filter_map(|item| match item {
            Value::String(name) => Some(SecretDecl {
                name: name.clone(),
                prompt: None,
            }),
            Value::Object(map) => text(map, &["name", "key", "env"]).map(|name| SecretDecl {
                name,
                prompt: text(map, &["prompt", "description", "help"]),
            }),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn meta(value: &Value) -> Meta {
        Meta::of(value.as_object().unwrap())
    }

    #[test]
    fn gates_and_lists_are_typed() {
        let m = meta(&json!({
            "platforms": ["macOS", "linux"],
            "metadata": {"prometheus": {"fallback_for_tools": "web_search", "session_platforms": []}}
        }));
        assert_eq!(m.platforms, ["macos", "linux"]);
        assert_eq!(m.fallback_for_tools, ["web_search"]);
        assert_eq!(m.session_platforms, None);
    }

    #[test]
    fn secrets_have_three_spellings() {
        let new = meta(
            &json!({"metadata": {"prometheus": {"secrets": [{"name": "NOTION_API_KEY", "prompt": "Notion key"}]}}}),
        );
        assert_eq!(new.secrets[0].name, "NOTION_API_KEY");
        assert_eq!(new.secrets[0].prompt.as_deref(), Some("Notion key"));
        let alias = meta(&json!({"required_environment_variables": ["A"]}));
        assert_eq!(alias.secrets[0].name, "A");
        let legacy = meta(&json!({"prerequisites": {"env_vars": ["B"]}}));
        assert_eq!(legacy.secrets[0].name, "B");
    }

    #[test]
    fn config_takes_a_list_or_a_map() {
        let list = meta(&json!({"config": [{"key": "region", "default": "eu"}]}));
        assert_eq!(list.config[0].key, "region");
        assert_eq!(list.config[0].default, Some(json!("eu")));
        let map = meta(&json!({"config": {"units": {"description": "metric or imperial"}}}));
        assert_eq!(
            map.config[0].description.as_deref(),
            Some("metric or imperial")
        );
    }
}

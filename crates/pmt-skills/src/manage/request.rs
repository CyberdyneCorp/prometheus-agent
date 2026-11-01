//! `skill_manage` arguments and their validation (spec: skills,
//! `skill_manage` tool validation).

use std::path::{Component, Path};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::frontmatter::{self, Document, MAX_SKILL_CHARS};
use crate::layout::SUPPORT_DIRS;

/// Largest supporting file.
pub const MAX_FILE_BYTES: usize = 1024 * 1024;

/// The six actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// A new skill.
    Create,
    /// Replace `SKILL.md`.
    Edit,
    /// Replace a string in `SKILL.md` or a supporting file.
    Patch,
    /// Remove the skill.
    Delete,
    /// Write a supporting file.
    WriteFile,
    /// Remove a supporting file.
    RemoveFile,
}

impl Action {
    /// The names, in the order the unknown-action error lists them.
    pub const NAMES: [&'static str; 6] = [
        "create",
        "edit",
        "patch",
        "delete",
        "write_file",
        "remove_file",
    ];

    fn parse(text: &str) -> Result<Self, String> {
        Ok(match text {
            "create" => Self::Create,
            "edit" => Self::Edit,
            "patch" => Self::Patch,
            "delete" => Self::Delete,
            "write_file" => Self::WriteFile,
            "remove_file" => Self::RemoveFile,
            other => {
                return Err(format!(
                    "Unknown action '{other}'. Use: {}",
                    Self::NAMES.join(", ")
                ));
            }
        })
    }

    /// The action's name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        Self::NAMES[self as usize]
    }
}

/// A validated call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    /// What to do.
    pub action: Action,
    /// The skill.
    pub name: String,
    /// Category directory of a new skill.
    pub category: Option<String>,
    /// `SKILL.md` text (create, edit).
    pub content: Option<String>,
    /// Text to replace (patch).
    pub old_string: Option<String>,
    /// Replacement (patch).
    pub new_string: Option<String>,
    /// Replace every match (patch).
    pub replace_all: bool,
    /// Supporting file (patch, `write_file`, `remove_file`).
    pub file_path: Option<String>,
    /// Its new content (`write_file`).
    pub file_content: Option<String>,
}

fn text(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn need<'a>(value: Option<&'a String>, key: &str, action: Action) -> Result<&'a String, String> {
    value.ok_or_else(|| format!("'{key}' is required for {}", action.as_str()))
}

impl Request {
    /// Parses and validates tool arguments.
    ///
    /// # Errors
    ///
    /// The message for the model.
    pub fn parse(args: &Value) -> Result<Self, String> {
        let action = Action::parse(&text(args, "action").unwrap_or_default())?;
        let request = Self {
            action,
            name: text(args, "name").unwrap_or_default(),
            category: text(args, "category").filter(|c| !c.is_empty()),
            content: text(args, "content"),
            old_string: text(args, "old_string"),
            new_string: text(args, "new_string"),
            replace_all: args
                .get("replace_all")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            file_path: text(args, "file_path").filter(|p| !p.is_empty()),
            file_content: text(args, "file_content"),
        };
        request.validate()?;
        Ok(request)
    }

    fn validate(&self) -> Result<(), String> {
        check_name("name", &self.name)?;
        if let Some(category) = &self.category {
            check_name("category", category)?;
        }
        match self.action {
            Action::Create | Action::Edit => {
                check_skill_md(need(self.content.as_ref(), "content", self.action)?)?;
            }
            Action::Patch => {
                need(self.old_string.as_ref(), "old_string", self.action)?;
                need(self.new_string.as_ref(), "new_string", self.action)?;
                if let Some(path) = &self.file_path {
                    check_support_path(path)?;
                }
            }
            Action::WriteFile => {
                check_support_path(need(self.file_path.as_ref(), "file_path", self.action)?)?;
                let content = need(self.file_content.as_ref(), "file_content", self.action)?;
                check_file_size(content)?;
            }
            Action::RemoveFile => {
                check_support_path(need(self.file_path.as_ref(), "file_path", self.action)?)?;
            }
            Action::Delete => {}
        }
        Ok(())
    }

    /// The file a content change targets.
    #[must_use]
    pub fn target_file(&self) -> &str {
        self.file_path
            .as_deref()
            .unwrap_or(crate::layout::SKILL_FILE)
    }
}

/// A skill or category name: at most 64 characters of
/// `^[a-z0-9][a-z0-9._-]*$`.
///
/// # Errors
///
/// The message for the model.
pub fn check_name(what: &str, name: &str) -> Result<(), String> {
    if frontmatter::valid_name(name) {
        Ok(())
    } else {
        Err(format!(
            "invalid {what} '{name}': at most 64 characters matching ^[a-z0-9][a-z0-9._-]*$"
        ))
    }
}

/// A complete `SKILL.md`: frontmatter with `name` and `description`, a
/// non-empty body, at most 100,000 characters.
///
/// # Errors
///
/// The message for the model.
pub fn check_skill_md(text: &str) -> Result<Document, String> {
    if text.chars().count() > MAX_SKILL_CHARS {
        return Err(format!("SKILL.md exceeds {MAX_SKILL_CHARS} characters"));
    }
    let doc =
        frontmatter::parse(text).map_err(|e| format!("SKILL.md line {}: {}", e.line, e.reason))?;
    if doc.body.trim().is_empty() {
        return Err("SKILL.md needs a body after the frontmatter".to_owned());
    }
    Ok(doc)
}

/// A supporting file path: relative, under one of the supporting
/// directories, no `..`.
///
/// # Errors
///
/// The message for the model.
pub fn check_support_path(path: &str) -> Result<(), String> {
    let mut components = Path::new(path).components();
    let first = components.next();
    let plain = Path::new(path)
        .components()
        .all(|c| matches!(c, Component::Normal(_)));
    let allowed =
        matches!(first, Some(Component::Normal(dir)) if SUPPORT_DIRS.iter().any(|d| dir == *d));
    if plain && allowed && components.next().is_some() {
        Ok(())
    } else {
        Err(format!(
            "file_path '{path}' must be a relative path under {} without '..'",
            SUPPORT_DIRS.map(|d| format!("{d}/")).join(", ")
        ))
    }
}

/// At most 1 MiB.
///
/// # Errors
///
/// The message for the model.
pub fn check_file_size(content: &str) -> Result<(), String> {
    if content.len() > MAX_FILE_BYTES {
        Err(format!(
            "supporting files are limited to {MAX_FILE_BYTES} bytes"
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn unknown_action_lists_the_six() {
        let err = Request::parse(&json!({"action": "rename", "name": "x"})).unwrap_err();
        assert_eq!(
            err,
            "Unknown action 'rename'. Use: create, edit, patch, delete, write_file, remove_file"
        );
    }

    #[test]
    fn support_paths_stay_in_the_allowed_dirs() {
        assert!(check_support_path("references/a.md").is_ok());
        assert!(check_support_path("scripts/sub/run.sh").is_ok());
        for bad in [
            "../bar/x.md",
            "references/../../x",
            "x.md",
            "/etc/x",
            "references",
            "other/x",
        ] {
            assert!(check_support_path(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn names_and_skill_md_are_checked() {
        assert!(check_name("name", "ok-name.v2").is_ok());
        assert!(check_name("name", &"a".repeat(65)).is_err());
        assert!(check_name("category", "a/b").is_err());
        assert!(check_skill_md("---\nname: a\ndescription: d\n---\n\n").is_err());
        assert!(check_skill_md("---\nname: a\ndescription: d\n---\nbody\n").is_ok());
    }
}

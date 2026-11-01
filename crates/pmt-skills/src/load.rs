//! What loading a skill gives the model (spec: skills, Load-time template
//! substitution and inline shell; Skill secrets and settings;
//! `skills_list` and `skill_view` tools).
//!
//! The body is processed in a fixed order: template variables, then inline
//! shell (only when enabled and the skill's provenance allows it), then the
//! skill's settings and the names of its secrets are appended.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::time::Duration;

use pmt_config::schema::Config;
use serde_json::Value;

use crate::catalog::Skill;
use crate::inline;
use crate::layout::{SKILL_FILE, SUPPORT_DIRS};
use crate::secrets::{self, SecretsStatus};
use crate::session::SkillsSession;

/// A loaded skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loaded {
    /// The processed body.
    pub body: String,
    /// Supporting files by directory (`references` -> paths).
    pub linked_files: BTreeMap<String, Vec<String>>,
    /// The declared secrets.
    pub secrets: SecretsStatus,
}

/// Replaces `${PROMETHEUS_SKILL_DIR}`, `${PROMETHEUS_SESSION_ID}` and their
/// `HERMES_` aliases; any other `${...}` stays as written.
#[must_use]
pub fn substitute(body: &str, skill_dir: &str, session_id: &str) -> String {
    [
        ("${PROMETHEUS_SKILL_DIR}", skill_dir),
        ("${HERMES_SKILL_DIR}", skill_dir),
        ("${PROMETHEUS_SESSION_ID}", session_id),
        ("${HERMES_SESSION_ID}", session_id),
    ]
    .iter()
    .fold(body.to_owned(), |text, (token, value)| {
        text.replace(token, value)
    })
}

/// Supporting files grouped by their first directory.
#[must_use]
pub fn linked_files(files: &[String]) -> BTreeMap<String, Vec<String>> {
    let mut linked: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for file in files.iter().filter(|f| f.as_str() != SKILL_FILE) {
        let group = file
            .split_once('/')
            .map(|(dir, _)| dir)
            .filter(|dir| SUPPORT_DIRS.contains(dir))
            .unwrap_or("other");
        linked
            .entry(group.to_owned())
            .or_default()
            .push(file.clone());
    }
    linked
}

/// The `[Skill config]` block: each declared setting's configured value,
/// else its default.
#[must_use]
pub fn settings_block(skill: &Skill, config: &Config) -> Option<String> {
    if skill.meta.config.is_empty() {
        return None;
    }
    let configured = config.skills.config.get(&skill.name);
    let mut text = String::from("[Skill config]");
    for decl in &skill.meta.config {
        let value = configured
            .and_then(|c| c.get(&decl.key))
            .or(decl.default.as_ref());
        let shown = match value {
            None => "(not set)".to_owned(),
            Some(Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
        };
        let _ = write!(text, "\n{} = {shown}", decl.key);
    }
    Some(text)
}

/// Loads `skill` for `session` under `config`.
pub async fn load(session: &SkillsSession, skill: &Skill, config: &Config) -> Loaded {
    let settings = &config.skills;
    let dir_text = skill.dir.display();
    let mut body = if settings.template_vars {
        substitute(&skill.body, &dir_text, &session.setup().session_id)
    } else {
        skill.body.clone()
    };
    if settings.inline_shell
        && skill.provenance.allows_inline_shell()
        && inline::has_snippets(&body)
    {
        let timeout = Duration::from_millis(settings.inline_shell_timeout_ms);
        body = match skill.dir.path() {
            Some(dir) => {
                let shell = session.setup().inline_shell.as_deref();
                inline::expand(&body, shell, dir, timeout).await
            }
            None => inline::expand(&body, None, std::path::Path::new("."), timeout).await,
        };
    }
    if let Some(block) = settings_block(skill, config) {
        body = format!("{}\n\n{block}\n", body.trim_end());
    }
    let status = secrets::collect(session, &skill.name, &skill.meta.secrets).await;
    if let Some(note) = status.note() {
        body = format!("{}\n\n{note}\n", body.trim_end());
    }
    session.mark_loaded(&skill.name, status.names());
    Loaded {
        body,
        linked_files: linked_files(&skill.dir.files()),
        secrets: status,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_tokens_are_replaced_unknown_kept() {
        let out = substitute(
            "${PROMETHEUS_SKILL_DIR}/x ${HERMES_SESSION_ID} ${OTHER}",
            "/s",
            "ses_1",
        );
        assert_eq!(out, "/s/x ses_1 ${OTHER}");
    }

    #[test]
    fn files_group_by_support_dir() {
        let files = [
            "SKILL.md".to_owned(),
            "references/a.md".to_owned(),
            "scripts/run.py".to_owned(),
            "notes.txt".to_owned(),
        ];
        let linked = linked_files(&files);
        assert_eq!(linked["references"], ["references/a.md"]);
        assert_eq!(linked["other"], ["notes.txt"]);
        assert!(!linked.values().flatten().any(|f| f == "SKILL.md"));
    }
}

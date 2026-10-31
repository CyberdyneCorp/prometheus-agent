//! Skills as slash commands (spec: skills, Skill slash commands).
//!
//! Each enabled, gate-passing skill is `/<slug>`. The search-root
//! precedence settles slug collisions between skills, and a core command
//! name or alias always wins: such a skill stays reachable as
//! `/skill <name>` and the `/skills` listing says why.

use std::collections::BTreeMap;

use pmt_store::{CreatedBy, SkillActivity};

use crate::catalog::{Missing, Skill};
use crate::gates;
use crate::load;
use crate::session::SkillsSession;

/// The command that loads any skill by name.
pub const SKILL_COMMAND: &str = "skill";

/// `name` lower-cased with spaces and underscores as hyphens.
#[must_use]
pub fn slug(name: &str) -> String {
    name.to_lowercase().replace([' ', '_'], "-")
}

/// The skill slash commands of a session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SlashTable {
    /// slug -> skill name.
    pub commands: BTreeMap<String, String>,
    /// skill name -> its slug that a core command holds.
    pub unavailable: BTreeMap<String, String>,
}

impl SlashTable {
    /// The table of `skills` (precedence order) next to the `core` command
    /// names and aliases (with or without the leading `/`).
    #[must_use]
    pub fn new<'a>(skills: impl IntoIterator<Item = &'a Skill>, core: &[&str]) -> Self {
        let core: Vec<&str> = core.iter().map(|c| c.trim_start_matches('/')).collect();
        let mut table = Self::default();
        for skill in skills {
            let slug = slug(&skill.name);
            if core.contains(&slug.as_str()) || slug == SKILL_COMMAND {
                table.unavailable.insert(skill.name.clone(), slug);
            } else {
                table
                    .commands
                    .entry(slug)
                    .or_insert_with(|| skill.name.clone());
            }
        }
        table
    }

    /// The `/skills` listing note of a skill whose slug is taken.
    #[must_use]
    pub fn note(&self, skill: &str) -> Option<String> {
        self.unavailable.get(skill).map(|slug| {
            format!(
                "slash command /{slug} unavailable: name taken by a built-in command; use /{SKILL_COMMAND} {skill}"
            )
        })
    }
}

/// The session's table under the current config.
#[must_use]
pub fn table(session: &SkillsSession, core: &[&str]) -> SlashTable {
    let (catalog, config) = session.catalog();
    SlashTable::new(session.visible(&catalog, &config), core)
}

/// The user message of `/<slug> args` or `/skill <name> args`: the loaded
/// skill and the user's instruction. Counts a use.
///
/// # Errors
///
/// The message to show the user (unknown, ambiguous, disabled or
/// unsupported skill).
pub async fn invoke(session: &SkillsSession, name: &str, args: &str) -> Result<String, String> {
    let (catalog, config) = session.catalog();
    let skill = catalog.lookup(name).map_err(|missing| match missing {
        Missing::Ambiguous(paths) => Missing::ambiguous_message(name, &paths),
        Missing::NotFound => format!("unknown skill '{name}'"),
    })?;
    if let Some(hidden) = gates::hidden(&config.skills, &session.setup().view, &skill) {
        return Err(hidden.message(&skill.name));
    }
    let loaded = load::load(session, &skill, &config).await;
    if let Err(err) =
        session
            .store()
            .record_skill_activity(&skill.name, SkillActivity::Used, CreatedBy::User)
    {
        tracing::warn!(skill = %skill.name, error = %err, "skill usage not recorded");
    }
    let mut message = format!(
        "[The user invoked the skill \"{}\". Follow it.]\n\n{}",
        skill.name,
        loaded.body.trim_end()
    );
    if !args.trim().is_empty() {
        message.push_str("\n\nUser instruction: ");
        message.push_str(args.trim());
    }
    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_lower_case_and_hyphenate() {
        assert_eq!(slug("Git_Workflow tools"), "git-workflow-tools");
    }
}

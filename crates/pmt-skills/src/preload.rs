//! Skills loaded before a session's first turn (spec: skills, Skill
//! preload at launch and per-profile auto-load).
//!
//! `prometheus chat -s a,b` names skills for one launch and
//! `skills.auto_load` names them for every session of the profile. They are
//! loaded once, at session start, into the text of the prompt's
//! `pinned_skills` layer; the session keeps that text for its whole life
//! (model switches and compression rebuild the prompt from the same text),
//! so preloading never breaks the prompt cache. A name that does not
//! resolve, is disabled or fails a gate is skipped with one warning.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use pmt_store::{CreatedBy, SkillActivity};

use crate::gates;
use crate::load;
use crate::session::SkillsSession;
use crate::slash;

/// A skipped name and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    /// The name as given.
    pub name: String,
    /// Why it did not load.
    pub reason: String,
    /// Named with `-s` (the CLI also prints `skill '<name>' not found`).
    pub explicit: bool,
}

/// What preloading gave.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Preloaded {
    /// The `pinned_skills` layer text (`None`: nothing loaded).
    pub text: Option<String>,
    /// The loaded skill names, in order.
    pub loaded: Vec<String>,
    /// The names that did not load.
    pub skipped: Vec<Skipped>,
}

impl Preloaded {
    /// The lines the CLI prints before the session starts: one
    /// `skill '<name>' not found` per explicit name that does not resolve.
    #[must_use]
    pub fn not_found_lines(&self) -> Vec<String> {
        self.skipped
            .iter()
            .filter(|s| s.explicit && s.reason.starts_with("unknown skill"))
            .map(|s| format!("skill '{}' not found", s.name))
            .collect()
    }
}

/// The names to preload: `explicit` (from `-s`, comma lists allowed)
/// first, then `skills.auto_load`, each once.
#[must_use]
pub fn names(explicit: &[String], auto_load: &[String]) -> Vec<(String, bool)> {
    let mut seen = BTreeSet::new();
    let split = |list: &[String]| -> Vec<String> {
        list.iter()
            .flat_map(|item| item.split(','))
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(str::to_owned)
            .collect()
    };
    let explicit = split(explicit).into_iter().map(|n| (n, true));
    let auto = split(auto_load).into_iter().map(|n| (n, false));
    explicit
        .chain(auto)
        .filter(|(name, _)| seen.insert(name.clone()))
        .collect()
}

/// Loads the session's preloaded skills: `explicit` from the launch plus
/// the profile's `skills.auto_load`. Each loaded skill counts as used.
pub async fn preload(session: &SkillsSession, explicit: &[String]) -> Preloaded {
    let (catalog, config) = session.catalog();
    let mut out = Preloaded::default();
    let mut text = String::new();
    for (name, is_explicit) in names(explicit, &config.skills.auto_load) {
        let resolved = slash::resolve(session, &catalog, &config, &name).and_then(|skill| {
            if gates::conditions_ok(&session.setup().view, &skill) {
                Ok(skill)
            } else {
                Err(format!(
                    "skill '{}' needs tools or a surface this session does not have",
                    skill.name
                ))
            }
        });
        let skill = match resolved {
            Ok(skill) => skill,
            Err(reason) => {
                tracing::warn!(skill = %name, reason = %reason, "preloaded skill skipped");
                out.skipped.push(Skipped {
                    name,
                    reason,
                    explicit: is_explicit,
                });
                continue;
            }
        };
        let loaded = load::load(session, &skill, &config).await;
        let _ = write!(
            text,
            "{}## Skill: {}\n\n{}",
            if text.is_empty() { "" } else { "\n\n" },
            skill.name,
            loaded.body.trim()
        );
        if let Err(err) =
            session
                .store()
                .record_skill_activity(&skill.name, SkillActivity::Used, CreatedBy::User)
        {
            tracing::warn!(skill = %skill.name, error = %err, "skill usage not recorded");
        }
        out.loaded.push(skill.name.clone());
    }
    if !text.is_empty() {
        out.text = Some(format!(
            "# Preloaded skills\n\nThese skills are loaded for this whole session; follow them where they apply.\n\n{text}"
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_names_come_first_and_load_once() {
        let got = names(
            &["a,b".to_owned(), "c".to_owned()],
            &["b".to_owned(), "d".to_owned()],
        );
        let expected: Vec<(String, bool)> = vec![
            ("a".into(), true),
            ("b".into(), true),
            ("c".into(), true),
            ("d".into(), false),
        ];
        assert_eq!(got, expected);
    }
}

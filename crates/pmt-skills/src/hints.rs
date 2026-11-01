//! Not-found messages that point to the right profile (spec: skills,
//! Skill lookup and wrong-profile hints).
//!
//! Profiles are islands: a skill of profile `work` is never loaded in
//! `default`. When the user asks for one anyway, the error names the
//! profile that has it instead of a bare "not found".

use std::fs;
use std::path::Path;

use crate::frontmatter;
use crate::layout::{self, SKILL_FILE};
use crate::roots::profile_skills;

/// `Skill '<name>' not found in profile '<profile>'.` plus the hint.
#[must_use]
pub fn not_found(home: &Path, profile: &str, name: &str) -> String {
    let hint = other_profile(home, profile, name).map_or_else(
        || "Use skills_list() to see available skills.".to_owned(),
        |other| format!("It exists in profile '{other}' (prometheus -p {other})."),
    );
    format!("Skill '{name}' not found in profile '{profile}'. {hint}")
}

/// The first other profile of `home` whose skills directory has `name`
/// (directory name, categorized path or frontmatter name).
#[must_use]
pub fn other_profile(home: &Path, current: &str, name: &str) -> Option<String> {
    let mut profiles: Vec<String> = fs::read_dir(home.join("profiles"))
        .ok()?
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|p| p != current && !p.starts_with('.'))
        .collect();
    profiles.sort();
    profiles
        .into_iter()
        .find(|p| has_skill(&profile_skills(&home.join("profiles").join(p)), name))
}

fn has_skill(skills_dir: &Path, name: &str) -> bool {
    let files = layout::walk(skills_dir);
    layout::skill_dirs(files.iter().map(String::as_str))
        .iter()
        .any(|rel| {
            rel == name
                || rel.rsplit('/').next() == Some(name)
                || fs::read_to_string(skills_dir.join(rel).join(SKILL_FILE))
                    .ok()
                    .and_then(|text| frontmatter::parse(&text).ok())
                    .is_some_and(|doc| doc.name == name)
        })
}

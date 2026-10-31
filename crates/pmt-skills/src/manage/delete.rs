//! `skill_manage(action="delete")` (spec: skills, Skill mutation safety
//! and fail-closed scanning; Autonomous skill creation and improvement).
//!
//! Only a plain directory inside the profile skills directory is ever
//! deleted. Symlinked directories, anything outside it, bundled and hub
//! skills, the essential skill and pinned skills are refused; a background
//! review archives instead of deleting.

use std::fs;

use pmt_config::essential::ESSENTIAL_SKILL;
use pmt_store::SkillState;

use super::{Ctx, Outcome, inside, mutate, read_only};
use crate::roots::Tier;

pub(super) fn delete(ctx: &Ctx<'_>, name: &str) -> Result<Outcome, String> {
    let skill = ctx.find(name)?;
    if skill.name == ESSENTIAL_SKILL {
        return Err(format!(
            "The essential skill '{ESSENTIAL_SKILL}' cannot be deleted."
        ));
    }
    let dir = match (skill.tier, skill.dir.path()) {
        (Tier::Profile, Some(dir)) if !skill.provenance.is_hub() => dir.to_path_buf(),
        _ => return Err(read_only(&skill)),
    };
    let link = fs::symlink_metadata(&dir).map_or(true, |m| m.file_type().is_symlink());
    if link {
        return Err(format!(
            "Skill '{}' is a symbolic link; it is not deleted by skill_manage.",
            skill.name
        ));
    }
    if !inside(&ctx.skills_dir, &dir) {
        return Err(read_only(&skill));
    }
    let pinned = ctx
        .session
        .store()
        .skill_usage(&skill.name)
        .map_err(|e| e.message().to_owned())?
        .is_some_and(|u| u.pinned);
    if pinned {
        return Err(format!(
            "Skill '{}' is pinned and cannot be deleted.",
            skill.name
        ));
    }
    let _lock = mutate::lock(&ctx.skills_dir, &skill.name)?;
    if ctx.session.review().is_some() {
        let archived = mutate::archive(&ctx.skills_dir, &skill.name, &dir, "")?;
        if let Err(err) = ctx
            .session
            .store()
            .set_skill_state(&skill.name, SkillState::Archived)
        {
            tracing::warn!(skill = %skill.name, error = %err, "skill state not recorded");
        }
        return Ok(Outcome::message(format!(
            "Archived skill '{}' to {}.",
            skill.name,
            ctx.display(&archived)
        )));
    }
    fs::remove_dir_all(&dir).map_err(|e| format!("{}: {e}", ctx.display(&dir)))?;
    Ok(Outcome::message(format!("Deleted skill '{}'.", skill.name)))
}

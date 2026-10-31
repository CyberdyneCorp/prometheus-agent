//! `skill_manage`: the agent's writes to its own skills (spec: skills,
//! `skill_manage` tool validation; Description budget and advisory lint on
//! agent writes; Skill mutation safety and fail-closed scanning; Skill
//! lookup and wrong-profile hints; Autonomous skill creation and
//! improvement).
//!
//! The agent writes only below `<profile>/skills/`: bundled, hub, plugin,
//! project and external skills are read-only to it. Every mutation runs
//! under the skill's lock and is undone when the security scan refuses the
//! result or fails. A background review ([`crate::review`]) may overwrite
//! only what it read and archives instead of deleting.

mod delete;
pub mod mutate;
pub mod request;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pmt_config::schema::Config;
use pmt_store::{CreatedBy, SkillActivity};
use serde::Serialize;

pub use request::{Action, Request};

use crate::catalog::{Catalog, Missing, Skill};
use crate::frontmatter::Document;
use crate::hints;
use crate::layout::SKILL_FILE;
use crate::lint::{self, LintContext};
use crate::pending;
use crate::review;
use crate::roots::Tier;
use crate::scan::Verdict;
use crate::session::SkillsSession;
use mutate::Change;

/// A successful call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Outcome {
    /// What happened.
    pub message: String,
    /// The skill directory, relative to the Prometheus home.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Lint findings this write introduced (advice only).
    pub lint_warnings: Vec<lint::Finding>,
    /// The scan verdict of the written skill.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verdict: Option<Verdict>,
    /// The pending id when the write was staged for approval.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_id: Option<String>,
}

impl Outcome {
    fn message(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            path: None,
            lint_warnings: Vec::new(),
            verdict: None,
            pending_id: None,
        }
    }
}

/// What one call works with.
pub(crate) struct Ctx<'a> {
    pub session: &'a SkillsSession,
    pub catalog: Catalog,
    pub config: Config,
    pub skills_dir: PathBuf,
}

impl Ctx<'_> {
    fn description_chars(&self) -> usize {
        usize::try_from(self.config.skills.index_description_chars).unwrap_or(usize::MAX)
    }

    fn display(&self, path: &Path) -> String {
        path.strip_prefix(self.session.scope().home())
            .unwrap_or(path)
            .display()
            .to_string()
    }

    /// The skill `name`, or the not-found / ambiguity message.
    pub(crate) fn find(&self, name: &str) -> Result<Arc<Skill>, String> {
        self.catalog.lookup(name).map_err(|missing| match missing {
            Missing::Ambiguous(paths) => Missing::ambiguous_message(name, &paths),
            Missing::NotFound => hints::not_found(
                self.session.scope().home(),
                self.session.scope().name().as_str(),
                name,
            ),
        })
    }

    /// The skill `name` when the agent may change it: a profile skill
    /// that is not hub-installed, inside the profile skills directory.
    fn writable(&self, name: &str) -> Result<(Arc<Skill>, PathBuf), String> {
        let skill = self.find(name)?;
        let dir = match (skill.tier, skill.dir.path()) {
            (Tier::Profile, Some(dir)) if !skill.provenance.is_hub() => dir.to_path_buf(),
            _ => return Err(read_only(&skill)),
        };
        if !inside(&self.skills_dir, &dir) {
            return Err(read_only(&skill));
        }
        Ok((skill, dir))
    }

    fn lint(&self, text: Option<&str>, dir_name: &str) -> Vec<lint::Finding> {
        let Some(text) = text else {
            return Vec::new();
        };
        let resolves = |n: &str| self.catalog.lookup(n).is_ok();
        lint::lint(
            text,
            &LintContext {
                dir_name,
                description_chars: self.description_chars(),
                resolves: &resolves,
            },
        )
    }

    /// Refuses a new or changed description over the index budget.
    fn check_description(&self, new: &Document, old: Option<&str>) -> Result<(), String> {
        let chars = new.description.chars().count();
        if old == Some(new.description.as_str()) || chars <= self.description_chars() {
            return Ok(());
        }
        Err(format!(
            "description has {chars} characters; the skills index allows {} (skills.index_description_chars). Shorten it.",
            self.description_chars()
        ))
    }

    /// In a review, an existing file may be overwritten only after it was
    /// read with `skill_view`.
    fn check_viewed(&self, skill: &str, file: &str, exists: bool) -> Result<(), String> {
        match self.session.review() {
            Some(state) if exists && !state.viewed(skill, file) => {
                Err(review::view_first(skill, file))
            }
            _ => Ok(()),
        }
    }

    fn record(&self, name: &str, created_by: CreatedBy) {
        if let Err(err) =
            self.session
                .store()
                .record_skill_activity(name, SkillActivity::Patched, created_by)
        {
            tracing::warn!(skill = %name, error = %err, "skill usage not recorded");
        }
    }
}

fn read_only(skill: &Skill) -> String {
    match skill.tier {
        Tier::Bundled => format!(
            "Skill '{}' is bundled and read-only; eject an editable copy with prometheus skills eject {}.",
            skill.name, skill.name
        ),
        _ if skill.provenance.is_hub() => format!(
            "Skill '{}' was installed from a hub and is not changed by skill_manage.",
            skill.name
        ),
        _ => format!(
            "Skill '{}' is outside the profile skills directory ({}) and is not changed by skill_manage.",
            skill.name,
            skill.tier.as_str()
        ),
    }
}

/// Whether `dir` (canonical) is below `root` (canonical).
pub(crate) fn inside(root: &Path, dir: &Path) -> bool {
    match (root.canonicalize(), dir.canonicalize()) {
        (Ok(root), Ok(dir)) => dir.starts_with(&root) && dir != root,
        _ => false,
    }
}

/// Runs a validated request; `approved` applies a staged write.
///
/// # Errors
///
/// The message for the model (nothing was changed).
pub fn run(session: &SkillsSession, request: &Request, approved: bool) -> Result<Outcome, String> {
    let (catalog, config) = session.catalog();
    let ctx = Ctx {
        session,
        catalog,
        config,
        skills_dir: session.skills_dir(),
    };
    if ctx.config.skills.write_approval && !approved {
        let id = pending::stage(session, request).map_err(|e| e.message().to_owned())?;
        let mut outcome = Outcome::message(format!(
            "Staged {} of '{}' as pending write {id}; it applies when the user runs /skills approve {id}.",
            request.action.as_str(),
            request.name
        ));
        outcome.pending_id = Some(id);
        return Ok(outcome);
    }
    match request.action {
        Action::Create => create(&ctx, request),
        Action::Edit => edit(&ctx, request),
        Action::Patch => patch(&ctx, request),
        Action::WriteFile => write_file(&ctx, request),
        Action::RemoveFile => remove_file(&ctx, request),
        Action::Delete => delete::delete(&ctx, &request.name),
    }
}

fn create(ctx: &Ctx<'_>, request: &Request) -> Result<Outcome, String> {
    let content = request.content.as_deref().unwrap_or_default();
    let doc = request::check_skill_md(content)?;
    if let Ok(existing) = ctx.catalog.lookup(&request.name) {
        return Err(format!(
            "Skill '{}' already exists ({}); use edit or patch to change it.",
            request.name,
            existing.tier.as_str()
        ));
    }
    ctx.check_description(&doc, None)?;
    let mut dir = ctx.skills_dir.clone();
    if let Some(category) = &request.category {
        dir.push(category);
    }
    dir.push(&request.name);
    if dir.exists() {
        return Err(format!("{} already exists", ctx.display(&dir)));
    }
    let _lock = mutate::lock(&ctx.skills_dir, &request.name)?;
    let changes = [Change::Write(SKILL_FILE.to_owned(), content.to_owned())];
    let report = mutate::commit(&dir, &changes, ctx.session.setup().scanner.as_ref())?;
    ctx.record(&doc.name, CreatedBy::Agent);
    Ok(Outcome {
        message: format!("Created skill '{}'.", doc.name),
        path: Some(ctx.display(&dir)),
        lint_warnings: ctx.lint(Some(content), &request.name),
        verdict: Some(report.verdict),
        pending_id: None,
    })
}

fn edit(ctx: &Ctx<'_>, request: &Request) -> Result<Outcome, String> {
    let content = request.content.as_deref().unwrap_or_default();
    let doc = request::check_skill_md(content)?;
    let (skill, dir) = ctx.writable(&request.name)?;
    ctx.check_viewed(&skill.name, SKILL_FILE, true)?;
    ctx.check_description(&doc, Some(&skill.description))?;
    let _lock = mutate::lock(&ctx.skills_dir, &skill.name)?;
    let before = std::fs::read_to_string(dir.join(SKILL_FILE)).ok();
    let changes = [Change::Write(SKILL_FILE.to_owned(), content.to_owned())];
    finish(
        ctx,
        &skill,
        &dir,
        &changes,
        before.as_deref(),
        Some(content),
    )
}

fn patch(ctx: &Ctx<'_>, request: &Request) -> Result<Outcome, String> {
    let (skill, dir) = ctx.writable(&request.name)?;
    let file = request.target_file().to_owned();
    ctx.check_viewed(&skill.name, &file, true)?;
    let _lock = mutate::lock(&ctx.skills_dir, &skill.name)?;
    let current = std::fs::read_to_string(dir.join(&file)).map_err(|e| format!("{file}: {e}"))?;
    let old = request.old_string.as_deref().unwrap_or_default();
    let new = request.new_string.as_deref().unwrap_or_default();
    let updated = replace(&current, old, new, request.replace_all)?;
    if file == SKILL_FILE {
        let doc = request::check_skill_md(&updated)?;
        ctx.check_description(&doc, Some(&skill.description))?;
    } else {
        request::check_file_size(&updated)?;
    }
    let changes = [Change::Write(file.clone(), updated.clone())];
    let (before, after) = if file == SKILL_FILE {
        (Some(current.as_str()), Some(updated.as_str()))
    } else {
        (None, None)
    };
    finish(ctx, &skill, &dir, &changes, before, after)
}

/// `current` with `old` replaced by `new`: exactly one match unless
/// `all`.
fn replace(current: &str, old: &str, new: &str, all: bool) -> Result<String, String> {
    if old.is_empty() {
        return Err("old_string must not be empty".to_owned());
    }
    match current.matches(old).count() {
        0 => Err("old_string was not found".to_owned()),
        1 => Ok(current.replacen(old, new, 1)),
        _ if all => Ok(current.replace(old, new)),
        n => Err(format!(
            "old_string matches {n} times; add surrounding text to make it unique or set replace_all"
        )),
    }
}

fn write_file(ctx: &Ctx<'_>, request: &Request) -> Result<Outcome, String> {
    let (skill, dir) = ctx.writable(&request.name)?;
    let file = request.target_file().to_owned();
    ctx.check_viewed(&skill.name, &file, dir.join(&file).exists())?;
    let _lock = mutate::lock(&ctx.skills_dir, &skill.name)?;
    let content = request.file_content.clone().unwrap_or_default();
    finish(
        ctx,
        &skill,
        &dir,
        &[Change::Write(file, content)],
        None,
        None,
    )
}

fn remove_file(ctx: &Ctx<'_>, request: &Request) -> Result<Outcome, String> {
    let (skill, dir) = ctx.writable(&request.name)?;
    let file = request.target_file().to_owned();
    let path = dir.join(&file);
    if !path.is_file() {
        return Err(format!("{file} does not exist in skill '{}'", skill.name));
    }
    let _lock = mutate::lock(&ctx.skills_dir, &skill.name)?;
    if ctx.session.review().is_some() {
        let archived = mutate::archive(&ctx.skills_dir, &skill.name, &path, &file)?;
        ctx.record(&skill.name, CreatedBy::User);
        return Ok(Outcome::message(format!(
            "Archived {file} of '{}' to {}.",
            skill.name,
            ctx.display(&archived)
        )));
    }
    finish(ctx, &skill, &dir, &[Change::Remove(file)], None, None)
}

/// Commits `changes` and builds the outcome; lint warnings compare the
/// `SKILL.md` text before and after (none for supporting files).
fn finish(
    ctx: &Ctx<'_>,
    skill: &Skill,
    dir: &Path,
    changes: &[Change],
    before: Option<&str>,
    after: Option<&str>,
) -> Result<Outcome, String> {
    let report = mutate::commit(dir, changes, ctx.session.setup().scanner.as_ref())?;
    ctx.record(&skill.name, CreatedBy::User);
    let warnings = lint::introduced(
        &ctx.lint(before, &skill.dir_name),
        &ctx.lint(after, &skill.dir_name),
    );
    Ok(Outcome {
        message: format!("Updated skill '{}'.", skill.name),
        path: Some(ctx.display(dir)),
        lint_warnings: warnings,
        verdict: Some(report.verdict),
        pending_id: None,
    })
}

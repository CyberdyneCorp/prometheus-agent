//! Staged skill writes (spec: skills, Skill write-approval review surface;
//! Skill mutation safety and fail-closed scanning).
//!
//! With `skills.write_approval = true`, `skill_manage` (foreground turn or
//! background review) stores its validated request as
//! `<profile>/pending/skills/<id>.json` instead of writing, so the record
//! survives restarts. The user lists them (`/skills pending`), reads the
//! change (`/skills diff <id>`) and applies or drops it (`/skills approve
//! <id>|all`, `/skills reject <id>|all`). An approved write goes through the
//! same checks and scan as a direct one.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use pmt_core::error::{Error, codes};
use pmt_core::private_fs;
use serde::{Deserialize, Serialize};
use similar::TextDiff;

use crate::layout::{self, SKILL_FILE};
use crate::lockfile::{io, now};
use crate::manage::{self, Action, Outcome, Request};
use crate::session::SkillsSession;

/// One staged write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    /// Its id.
    pub id: String,
    /// When it was staged (RFC 3339).
    pub created_at: String,
    /// The session that staged it.
    pub session_id: String,
    /// The validated request.
    pub request: Request,
}

impl Pending {
    /// One line for `/skills pending`: id, action, skill (and file), time.
    #[must_use]
    pub fn gist(&self) -> String {
        let request = &self.request;
        let file = match request.action {
            Action::Patch | Action::WriteFile | Action::RemoveFile => {
                format!(" {}", request.target_file())
            }
            _ => String::new(),
        };
        format!(
            "{}  {} {}{file}  ({})",
            self.id,
            request.action.as_str(),
            request.name,
            self.created_at
        )
    }
}

/// `<profile>/pending/skills`.
#[must_use]
pub fn dir(profile_dir: &Path) -> PathBuf {
    profile_dir.join("pending").join("skills")
}

fn file(profile_dir: &Path, id: &str) -> Result<PathBuf, Error> {
    let valid = !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric());
    if !valid {
        return Err(not_found(id));
    }
    Ok(dir(profile_dir).join(format!("{id}.json")))
}

fn not_found(id: &str) -> Error {
    Error::new(
        &codes::SKILL_PENDING_NOT_FOUND,
        format!("no pending skill write '{id}'"),
    )
}

/// Stages `request`; returns its id.
///
/// # Errors
///
/// `skill.io`.
pub fn stage(session: &SkillsSession, request: &Request) -> Result<String, Error> {
    let profile = session.scope().dir();
    let id = ulid::Ulid::generate().to_string();
    let pending = Pending {
        id: id.clone(),
        created_at: now(),
        session_id: session.setup().session_id.clone(),
        request: request.clone(),
    };
    let dir = dir(profile);
    if let Some(parent) = dir.parent() {
        private_fs::create_dir(parent).map_err(|e| io(parent, &e))?;
    }
    private_fs::create_dir(&dir).map_err(|e| io(&dir, &e))?;
    let path = file(profile, &id)?;
    let text = serde_json::to_vec_pretty(&pending)
        .map_err(|e| Error::new(&codes::SKILL_IO, format!("pending write: {e}")))?;
    private_fs::write_atomic(&path, &text).map_err(|e| io(&path, &e))?;
    Ok(id)
}

/// Every staged write of the profile, oldest first.
#[must_use]
pub fn list(profile_dir: &Path) -> Vec<Pending> {
    let Ok(entries) = fs::read_dir(dir(profile_dir)) else {
        return Vec::new();
    };
    let mut all: Vec<Pending> = entries
        .flatten()
        .filter_map(|e| fs::read(e.path()).ok())
        .filter_map(|bytes| serde_json::from_slice(&bytes).ok())
        .collect();
    all.sort_by(|a, b| a.id.cmp(&b.id));
    all
}

/// Staged write `id`.
///
/// # Errors
///
/// `skill.pending_not_found`.
pub fn get(profile_dir: &Path, id: &str) -> Result<Pending, Error> {
    let path = file(profile_dir, id)?;
    fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .ok_or_else(|| not_found(id))
}

/// Applies staged write `id` (checks and scan included), then drops it.
///
/// # Errors
///
/// `skill.pending_not_found`; the write's own refusal as `skill.invalid`
/// (the staged record is kept so the user can reject it).
pub fn approve(session: &SkillsSession, id: &str) -> Result<Outcome, Error> {
    let pending = get(session.scope().dir(), id)?;
    let outcome = manage::run(session, &pending.request, true)
        .map_err(|why| Error::new(&codes::SKILL_INVALID, format!("{id}: {why}")))?;
    let path = file(session.scope().dir(), id)?;
    fs::remove_file(&path).map_err(|e| io(&path, &e))?;
    Ok(outcome)
}

/// Drops staged write `id`.
///
/// # Errors
///
/// `skill.pending_not_found`.
pub fn reject(profile_dir: &Path, id: &str) -> Result<(), Error> {
    let path = file(profile_dir, id)?;
    fs::remove_file(&path).map_err(|_| not_found(id))
}

/// The ids `target` names: one id, or every staged write for `all`.
#[must_use]
pub fn ids(profile_dir: &Path, target: &str) -> Vec<String> {
    if target == "all" {
        list(profile_dir).into_iter().map(|p| p.id).collect()
    } else {
        vec![target.to_owned()]
    }
}

/// The unified diff a staged write would make, from the skill as it is now.
///
/// # Errors
///
/// `skill.pending_not_found`.
pub fn diff(session: &SkillsSession, id: &str) -> Result<String, Error> {
    let pending = get(session.scope().dir(), id)?;
    let request = &pending.request;
    let current = current_dir(session, &request.name);
    let read = |rel: &str| {
        current
            .as_ref()
            .and_then(|dir| fs::read_to_string(dir.join(rel)).ok())
            .unwrap_or_default()
    };
    let mut out = String::new();
    for (rel, old, new) in changes(request, current.as_deref(), &read) {
        let label = format!("{}/{rel}", request.name);
        let _ = write!(
            out,
            "{}",
            TextDiff::from_lines(&old, &new)
                .unified_diff()
                .header(&format!("a/{label}"), &format!("b/{label}"))
        );
    }
    Ok(out)
}

/// `(file, before, after)` of each file `request` changes.
fn changes(
    request: &Request,
    current: Option<&Path>,
    read: &dyn Fn(&str) -> String,
) -> Vec<(String, String, String)> {
    let target = request.target_file().to_owned();
    let whole = |text: &Option<String>| text.clone().unwrap_or_default();
    match request.action {
        Action::Create | Action::Edit => vec![(
            SKILL_FILE.to_owned(),
            read(SKILL_FILE),
            whole(&request.content),
        )],
        Action::Patch => {
            let old = read(&target);
            let new = manage::replace(
                &old,
                request.old_string.as_deref().unwrap_or_default(),
                request.new_string.as_deref().unwrap_or_default(),
                request.replace_all,
            )
            .unwrap_or_else(|why| format!("(this patch no longer applies: {why})\n"));
            vec![(target, old, new)]
        }
        Action::WriteFile => vec![(target.clone(), read(&target), whole(&request.file_content))],
        Action::RemoveFile => vec![(target.clone(), read(&target), String::new())],
        Action::Delete => current
            .map(layout::walk)
            .unwrap_or_default()
            .into_iter()
            .map(|rel| {
                let old = read(&rel);
                (rel, old, String::new())
            })
            .collect(),
    }
}

/// The directory of skill `name` on disk, when it exists.
fn current_dir(session: &SkillsSession, name: &str) -> Option<PathBuf> {
    let (catalog, _) = session.catalog();
    catalog
        .lookup(name)
        .ok()
        .and_then(|s| s.dir.path().map(Path::to_path_buf))
}

/// `text` cut to `limit` characters with a pointer to the CLI when it is
/// longer (messaging surfaces).
#[must_use]
pub fn fit(text: &str, limit: Option<usize>, id: &str) -> String {
    let pointer =
        format!("\n[diff truncated; see all of it with: prometheus skills pending diff {id}]");
    match limit {
        Some(limit) if text.chars().count() > limit => {
            let keep = limit.saturating_sub(pointer.chars().count());
            let mut cut: String = text.chars().take(keep).collect();
            cut.push_str(&pointer);
            cut
        }
        _ => text.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_diffs_are_cut_with_a_pointer() {
        let text = "x".repeat(500);
        let cut = fit(&text, Some(200), "ID");
        assert_eq!(cut.chars().count(), 200);
        assert!(cut.ends_with("prometheus skills pending diff ID]"));
        assert_eq!(fit("short", Some(200), "ID"), "short");
        assert_eq!(fit(&text, None, "ID"), text);
    }
}

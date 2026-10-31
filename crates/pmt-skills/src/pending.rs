//! Staged skill writes (spec: skills, Skill mutation safety and
//! fail-closed scanning: `skills.write_approval`).
//!
//! With `skills.write_approval = true`, `skill_manage` stores its validated
//! request as `<profile>/skills/.pending/<id>.json` instead of writing; the
//! user reviews them with `/skills pending` and applies or drops one with
//! `/skills approve <id>` / `/skills reject <id>`. An approved write goes
//! through the same checks and scan as a direct one.

use std::fs;
use std::path::{Path, PathBuf};

use pmt_core::error::{Error, codes};
use pmt_core::private_fs;
use serde::{Deserialize, Serialize};

use crate::lockfile::{io, now};
use crate::manage::{self, Outcome, Request};
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

/// `<skills>/.pending`.
#[must_use]
pub fn dir(skills_dir: &Path) -> PathBuf {
    skills_dir.join(".pending")
}

fn file(skills_dir: &Path, id: &str) -> Result<PathBuf, Error> {
    let valid = !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric());
    if !valid {
        return Err(not_found(id));
    }
    Ok(dir(skills_dir).join(format!("{id}.json")))
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
    let skills = session.skills_dir();
    let id = ulid::Ulid::generate().to_string();
    let pending = Pending {
        id: id.clone(),
        created_at: now(),
        session_id: session.setup().session_id.clone(),
        request: request.clone(),
    };
    let dir = dir(&skills);
    private_fs::create_dir(&dir).map_err(|e| io(&dir, &e))?;
    let path = file(&skills, &id)?;
    let text = serde_json::to_vec_pretty(&pending)
        .map_err(|e| Error::new(&codes::SKILL_IO, format!("pending write: {e}")))?;
    private_fs::write_atomic(&path, &text).map_err(|e| io(&path, &e))?;
    Ok(id)
}

/// Every staged write, oldest first.
#[must_use]
pub fn list(skills_dir: &Path) -> Vec<Pending> {
    let Ok(entries) = fs::read_dir(dir(skills_dir)) else {
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

/// Applies staged write `id` (checks and scan included), then drops it.
///
/// # Errors
///
/// `skill.pending_not_found`; the write's own refusal as `skill.invalid`
/// (the staged record is kept so the user can reject it).
pub fn approve(session: &SkillsSession, id: &str) -> Result<Outcome, Error> {
    let path = file(&session.skills_dir(), id)?;
    let pending: Pending = fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .ok_or_else(|| not_found(id))?;
    let outcome = manage::run(session, &pending.request, true)
        .map_err(|why| Error::new(&codes::SKILL_INVALID, why))?;
    fs::remove_file(&path).map_err(|e| io(&path, &e))?;
    Ok(outcome)
}

/// Drops staged write `id`.
///
/// # Errors
///
/// `skill.pending_not_found`.
pub fn reject(skills_dir: &Path, id: &str) -> Result<(), Error> {
    let path = file(skills_dir, id)?;
    fs::remove_file(&path).map_err(|_| not_found(id))
}

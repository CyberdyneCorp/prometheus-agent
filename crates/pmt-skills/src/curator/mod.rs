//! The skill curator (spec: skills, Curator configuration and scheduling;
//! Curator transitions, scope and backups; Curator CLI; Curator audit
//! ledger, single-edit rollback, purge and reports).
//!
//! The curator keeps the agent's own skill library tidy: it manages only
//! skills the agent created (or the user adopted), never pinned ones or
//! ones a scheduled job uses, never hub, plugin, external or bundled
//! skills. A pass marks unused skills `stale`, reactivates stale ones used
//! again and archives long-unused ones into `skills/.archive/` (it never
//! deletes), after writing a backup of the skills tree; it can also merge
//! overlapping skills through a model review ([`consolidate`]).
//!
//! - [`plan`] decides the transitions; [`run::run`] applies them.
//! - [`moves`] archives, restores, prunes and purges with ledger entries.
//! - [`backup`] writes, rotates and restores tree backups.
//! - [`schedule`] decides when an automatic pass is due.

pub mod backup;
pub mod consolidate;
pub mod moves;
pub mod report;
pub mod run;
pub mod schedule;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use jiff::{SignedDuration, Timestamp};
use pmt_config::Config;
use pmt_store::Store;
use pmt_store::skill_usage::{CreatedBy, SkillState, SkillUsage};
use serde::Serialize;

use crate::catalog::{Catalog, Provenance, Skill};
use crate::roots::{Tier, profile_skills};

/// What the curator works on.
pub struct Scope<'a> {
    /// The profile directory.
    pub profile_dir: &'a Path,
    /// Its store.
    pub store: &'a Store,
    /// Its configuration.
    pub config: &'a Config,
    /// Its skills (without the project tier).
    pub catalog: &'a Catalog,
    /// Skills any scheduled job names (paused ones included).
    pub cron_skills: &'a BTreeSet<String>,
    /// Now.
    pub now: Timestamp,
}

impl Scope<'_> {
    /// The profile's `skills/`.
    #[must_use]
    pub fn skills_dir(&self) -> PathBuf {
        profile_skills(self.profile_dir)
    }
}

/// A skill under curator management.
#[derive(Debug, Clone)]
pub struct Managed {
    /// The skill.
    pub skill: Arc<Skill>,
    /// Its directory, relative to `skills/`.
    pub rel: String,
    /// Its state.
    pub state: SkillState,
    /// Its last activity (or its `SKILL.md` time without one).
    pub last_activity: Timestamp,
}

/// A skill left alone, with why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Skipped {
    /// The skill.
    pub name: String,
    /// `pinned`, `used by a scheduled job`.
    pub reason: String,
}

/// One planned transition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Transition {
    /// The skill.
    pub name: String,
    /// `active`, `stale`.
    pub from: String,
    /// `active`, `stale`, `archived`.
    pub to: String,
    /// Days without activity.
    pub idle_days: i64,
}

/// What a pass would do.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Plan {
    /// The transitions.
    pub transitions: Vec<Transition>,
    /// Managed skills left alone.
    pub skipped: Vec<Skipped>,
}

fn usage_of(store: &Store, name: &str) -> Option<SkillUsage> {
    store.skill_usage(name).ok().flatten()
}

/// Whether a skill is outside the curator's reach whatever its record
/// says: not in the profile directory, hub-installed, bundled, or an
/// ejected bundled copy (unless `curator.prune_builtins`).
fn out_of_reach(skill: &Skill, prune_builtins: bool) -> bool {
    skill.tier != Tier::Profile
        || match skill.provenance {
            Provenance::Local => false,
            Provenance::Builtin => !prune_builtins,
            Provenance::Hub(_) | Provenance::Unreadable => true,
        }
}

fn modified(skill: &Skill) -> Option<Timestamp> {
    let path = skill.dir.path()?.join(crate::layout::SKILL_FILE);
    let time = std::fs::metadata(path).ok()?.modified().ok()?;
    Timestamp::try_from(time).ok()
}

/// The managed skills, and the managed ones left alone with why.
#[must_use]
pub fn managed(scope: &Scope<'_>) -> (Vec<Managed>, Vec<Skipped>) {
    let skills_dir = scope.skills_dir();
    let prune_builtins = scope.config.curator.prune_builtins;
    let mut out = Vec::new();
    let mut skipped = Vec::new();
    for skill in &scope.catalog.skills {
        if out_of_reach(skill, prune_builtins) {
            continue;
        }
        let Some(usage) = usage_of(scope.store, &skill.name)
            .filter(|u| u.created_by == CreatedBy::Agent || u.adopted)
        else {
            continue;
        };
        if usage.pinned {
            skipped.push(skip(&skill.name, "pinned"));
            continue;
        }
        if scope.cron_skills.contains(&skill.name) {
            skipped.push(skip(&skill.name, "used by a scheduled job"));
            continue;
        }
        let Some(rel) = skill
            .dir
            .path()
            .and_then(|d| d.strip_prefix(&skills_dir).ok())
            .map(|r| r.to_string_lossy().replace('\\', "/"))
        else {
            continue;
        };
        let last_activity = usage
            .last_activity_at
            .or_else(|| modified(skill))
            .unwrap_or(scope.now);
        out.push(Managed {
            skill: Arc::clone(skill),
            rel,
            state: usage.state,
            last_activity,
        });
    }
    (out, skipped)
}

fn skip(name: &str, reason: &str) -> Skipped {
    Skipped {
        name: name.to_owned(),
        reason: reason.to_owned(),
    }
}

fn secs(value: u64) -> SignedDuration {
    SignedDuration::from_secs(i64::try_from(value).unwrap_or(i64::MAX))
}

/// The transitions a pass makes now.
#[must_use]
pub fn plan(scope: &Scope<'_>) -> Plan {
    let curator = &scope.config.curator;
    let (managed, skipped) = managed(scope);
    let mut transitions = Vec::new();
    for m in &managed {
        let idle = scope.now.duration_since(m.last_activity);
        let to = if idle >= secs(curator.archive_after_secs) {
            SkillState::Archived
        } else if idle >= secs(curator.stale_after_secs) {
            SkillState::Stale
        } else {
            SkillState::Active
        };
        if to != m.state {
            transitions.push(Transition {
                name: m.skill.name.clone(),
                from: m.state.as_str().to_owned(),
                to: to.as_str().to_owned(),
                idle_days: idle.as_secs() / 86_400,
            });
        }
    }
    Plan {
        transitions,
        skipped,
    }
}

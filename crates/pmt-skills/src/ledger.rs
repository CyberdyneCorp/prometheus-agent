//! The skill ledger seen from the skills directory (spec: skills, Curator
//! audit ledger, single-edit rollback, purge and reports): capture the
//! files a mutation will touch, record the entry once it is done, and
//! roll one entry back.
//!
//! Paths are relative to the profile's `skills/` directory, so one entry
//! can cover an archive move (`deploy/SKILL.md` gone,
//! `.archive/deploy/SKILL.md` created). Recording never blocks a
//! mutation (a failure is logged); a rollback records its own entry first
//! and does nothing when that fails.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use pmt_core::error::{Error, codes};
use pmt_core::private_fs;
use pmt_store::Store;
use pmt_store::skill_ledger::{Entry, FileChange, NewEntry};
use sha2::{Digest as _, Sha256};

use crate::layout;

/// Who made a change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Actor {
    /// The background curator.
    Curator,
    /// The agent (`skill_manage`, a review).
    Agent,
    /// The user (CLI, slash command).
    User,
}

impl Actor {
    /// The stored name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Curator => "curator",
            Self::Agent => "agent",
            Self::User => "user",
        }
    }
}

/// The files of one mutation as they were before it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Capture {
    files: BTreeMap<String, Option<Vec<u8>>>,
}

impl Capture {
    /// Adds paths that do not exist yet (what the mutation will create).
    #[must_use]
    pub fn with_new(mut self, rels: impl IntoIterator<Item = String>) -> Self {
        for rel in rels {
            self.files.entry(rel).or_insert(None);
        }
        self
    }

    /// The captured paths.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.files.keys().map(String::as_str)
    }
}

/// What one ledger write carries besides the files.
#[derive(Debug, Clone, Default)]
pub struct Evidence {
    /// The skill a consolidation merged this one into.
    pub absorbed_into: Option<String>,
    /// The session that made the change.
    pub session_id: Option<String>,
}

/// The ledger of one profile.
pub struct Ledger<'a> {
    store: &'a Store,
    skills_dir: PathBuf,
    max_bytes: u64,
}

fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut s, b| {
            use std::fmt::Write as _;
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// `rel` below `root`, `/`-separated.
fn join(root: &Path, rel: &str) -> PathBuf {
    rel.split('/').fold(root.to_path_buf(), |p, c| p.join(c))
}

impl<'a> Ledger<'a> {
    /// The ledger over `store` for `skills_dir`, compacted beyond
    /// `max_bytes` (`skills.ledger_max_bytes`, 0 = unbounded).
    #[must_use]
    pub fn new(store: &'a Store, skills_dir: &Path, max_bytes: u64) -> Self {
        Self {
            store,
            skills_dir: skills_dir.to_path_buf(),
            max_bytes,
        }
    }

    /// The skills directory.
    #[must_use]
    pub fn skills_dir(&self) -> &Path {
        &self.skills_dir
    }

    /// The current content of `rels` (missing files as `None`).
    #[must_use]
    pub fn capture(&self, rels: impl IntoIterator<Item = String>) -> Capture {
        Capture {
            files: rels
                .into_iter()
                .map(|rel| {
                    let bytes = std::fs::read(join(&self.skills_dir, &rel)).ok();
                    (rel, bytes)
                })
                .collect(),
        }
    }

    /// Every file below `dir_rel` (a skill directory), as it is now.
    #[must_use]
    pub fn capture_dir(&self, dir_rel: &str) -> Capture {
        let dir = join(&self.skills_dir, dir_rel);
        self.capture(
            layout::walk(&dir)
                .into_iter()
                .map(|rel| format!("{dir_rel}/{rel}")),
        )
    }

    /// The paths of every file below `dir_rel`, as they are now.
    #[must_use]
    pub fn files_below(&self, dir_rel: &str) -> Vec<String> {
        layout::walk(&join(&self.skills_dir, dir_rel))
            .into_iter()
            .map(|rel| format!("{dir_rel}/{rel}"))
            .collect()
    }

    /// Builds the entry of a finished mutation: every captured path with
    /// its content before and now.
    fn entry(
        &self,
        actor: Actor,
        action: &str,
        skill: &str,
        before: &Capture,
        evidence: &Evidence,
    ) -> (NewEntry, Vec<Vec<u8>>) {
        let mut contents = Vec::new();
        let mut files = Vec::new();
        for (rel, old) in &before.files {
            let now = std::fs::read(join(&self.skills_dir, rel)).ok();
            if *old == now {
                continue;
            }
            let digest = |b: &Option<Vec<u8>>| b.as_deref().map(sha);
            files.push(FileChange {
                path: rel.clone(),
                before: digest(old),
                after: digest(&now),
            });
            contents.extend(old.iter().cloned());
            contents.extend(now);
        }
        let entry = NewEntry {
            actor: actor.as_str().to_owned(),
            action: action.to_owned(),
            skill: skill.to_owned(),
            absorbed_into: evidence.absorbed_into.clone(),
            session_id: evidence.session_id.clone(),
            files,
        };
        (entry, contents)
    }

    /// Records a finished mutation; a failure is logged, never returned
    /// (the ledger never blocks a change). The entry id, when recorded.
    pub fn record(
        &self,
        actor: Actor,
        action: &str,
        skill: &str,
        before: &Capture,
        evidence: &Evidence,
    ) -> Option<i64> {
        let (entry, contents) = self.entry(actor, action, skill, before, evidence);
        match self
            .store
            .append_skill_ledger(&entry, &contents, self.max_bytes)
        {
            Ok(id) => Some(id),
            Err(err) => {
                tracing::warn!(skill = %skill, action = %action, error = %err, "skill ledger entry not recorded");
                None
            }
        }
    }

    /// Restores exactly the files entry `id` touched to their content
    /// before it (removing the files it created), after recording a
    /// `rollback` entry; nothing changes when that record fails.
    ///
    /// # Errors
    ///
    /// `curator.refused` for an unknown entry or a failed safety record,
    /// `skill.io` when a file cannot be restored.
    pub fn rollback(&self, id: i64) -> Result<Entry, Error> {
        let entry = self
            .store
            .skill_ledger_entry(id)?
            .ok_or_else(|| refused(format!("no ledger entry L{id}")))?;
        let mut targets = Vec::new();
        for change in &entry.files {
            let content = match &change.before {
                Some(sha) => Some(self.store.skill_ledger_content(sha).map_err(|e| {
                    refused(format!(
                        "L{id}: the content of {} is gone: {}",
                        change.path,
                        e.message()
                    ))
                })?),
                None => None,
            };
            targets.push((change.path.clone(), content));
        }
        let paths: BTreeSet<String> = targets.iter().map(|(p, _)| p.clone()).collect();
        let safety = self.capture(paths);
        let (mut safety_entry, mut contents) = entry_towards(&safety, &targets);
        Actor::User.as_str().clone_into(&mut safety_entry.actor);
        "rollback".clone_into(&mut safety_entry.action);
        safety_entry.skill.clone_from(&entry.skill);
        contents.extend(safety.files.values().flatten().cloned());
        self.store
            .append_skill_ledger(&safety_entry, &contents, self.max_bytes)
            .map_err(|e| {
                refused(format!(
                    "rollback refused: the safety record failed: {}",
                    e.message()
                ))
            })?;
        for (rel, content) in &targets {
            restore_file(&self.skills_dir, rel, content.as_deref())?;
        }
        Ok(entry)
    }
}

/// The entry of a change from `now` to `targets` (built before it is
/// made, for the rollback's safety record).
fn entry_towards(now: &Capture, targets: &[(String, Option<Vec<u8>>)]) -> (NewEntry, Vec<Vec<u8>>) {
    let mut files = Vec::new();
    let mut contents = Vec::new();
    for (rel, target) in targets {
        let current = now.files.get(rel).cloned().flatten();
        files.push(FileChange {
            path: rel.clone(),
            before: current.as_deref().map(sha),
            after: target.as_deref().map(sha),
        });
        contents.extend(target.iter().cloned());
    }
    let entry = NewEntry {
        files,
        ..NewEntry::default()
    };
    (entry, contents)
}

/// Writes `content` to `rel` (or removes it when `None`).
fn restore_file(root: &Path, rel: &str, content: Option<&[u8]>) -> Result<(), Error> {
    let path = join(root, rel);
    if let Some(bytes) = content {
        if let Some(parent) = path.parent() {
            private_fs::create_dir(parent).map_err(|e| Error::io(&codes::SKILL_IO, parent, &e))?;
        }
        return private_fs::write_atomic(&path, bytes)
            .map_err(|e| Error::io(&codes::SKILL_IO, &path, &e));
    }
    if path.exists() {
        std::fs::remove_file(&path).map_err(|e| Error::io(&codes::SKILL_IO, &path, &e))?;
    }
    remove_empty_parents(root, &path);
    Ok(())
}

fn refused(message: String) -> Error {
    Error::new(&codes::CURATOR_REFUSED, message)
}

/// Removes the now-empty directories between `path` and `root`.
fn remove_empty_parents(root: &Path, path: &Path) {
    let mut dir = path.parent();
    while let Some(current) = dir {
        if current == root || !current.starts_with(root) || std::fs::remove_dir(current).is_err() {
            break;
        }
        dir = current.parent();
    }
}

//! Pruning and editing journey nodes (spec: memory, Journey pruning and
//! editing). Deleting a skill archives it (restorable from
//! `skills/.archive/`); deleting a memory entry removes it as a journaled
//! `origin = user` revision that `prometheus memory revert` undoes.
//! Edits go through the same validation and scanning as any other write:
//! `skill_manage` edit for a skill's `SKILL.md`, the memory write path for
//! an entry.

use std::path::Path;
use std::sync::Arc;

use pmt_config::ProfileScope;
use pmt_config::essential::ESSENTIAL_SKILL;
use pmt_core::error::{Error, codes};
use pmt_memory::ops::{Action, Op};
use pmt_memory::write;
use pmt_skills::layout::SKILL_FILE;
use pmt_skills::manage::{self, mutate};
use pmt_skills::roots::{Tier, profile_skills};
use pmt_skills::session::{SessionSetup, SkillsSession};
use pmt_store::{MemoryEntry, MemoryOrigin, RevisionFilter, SkillState, Store};

use super::nodes::{Node, catalog};

/// The profile a journey change applies to.
pub struct Journey<'a> {
    /// The profile.
    pub scope: &'a ProfileScope,
    /// Its store.
    pub store: &'a Arc<Store>,
    /// The user's home (external skill roots).
    pub user_home: Option<&'a Path>,
}

impl Journey<'_> {
    /// What deleting `node` does, for the confirmation question.
    ///
    /// # Errors
    ///
    /// `skill.not_found` / `memory.not_found`.
    pub fn describe(&self, node: &Node) -> Result<String, Error> {
        Ok(match node {
            Node::Skill(name) => {
                self.skill(name)?;
                format!("archive skill '{name}' (restorable from skills/.archive/)")
            }
            Node::Memory(id) => format!(
                "remove memory entry {id}: {}",
                self.entry(id)?.content.trim()
            ),
        })
    }

    /// Deletes `node`; what was done.
    ///
    /// # Errors
    ///
    /// Unknown nodes, read-only or pinned skills, store and I/O errors.
    pub fn delete(&self, node: &Node) -> Result<String, Error> {
        match node {
            Node::Skill(name) => self.archive(name),
            Node::Memory(id) => self.remove(id),
        }
    }

    /// The text `node`'s edit starts from: the skill's `SKILL.md` or the
    /// entry's content.
    ///
    /// # Errors
    ///
    /// Unknown nodes; reading the skill file.
    pub fn text(&self, node: &Node) -> Result<String, Error> {
        match node {
            Node::Skill(name) => {
                let dir = self.skill(name)?;
                let path = dir.join(SKILL_FILE);
                std::fs::read_to_string(&path)
                    .map_err(|e| Error::new(&codes::SKILL_IO, format!("{}: {e}", path.display())))
            }
            Node::Memory(id) => Ok(self.entry(id)?.content),
        }
    }

    /// Applies `edited` to `node`; what was done.
    ///
    /// # Errors
    ///
    /// What validation or the scan refused (nothing changes then).
    pub fn apply(&self, node: &Node, edited: &str) -> Result<String, Error> {
        if edited.trim() == self.text(node)?.trim() {
            return Ok("no changes".to_owned());
        }
        match node {
            Node::Skill(name) => self.edit_skill(name, edited),
            Node::Memory(id) => self.patch(id, edited),
        }
    }

    fn entry(&self, id: &str) -> Result<MemoryEntry, Error> {
        self.store.memory_entry(id)?.ok_or_else(|| {
            Error::new(&codes::MEMORY_NOT_FOUND, format!("no memory entry '{id}'"))
                .with_hint("prometheus journey list")
        })
    }

    /// The directory of skill `name`, when the user may change it.
    fn skill(&self, name: &str) -> Result<std::path::PathBuf, Error> {
        let skill = catalog(self.scope, self.user_home)
            .lookup(name)
            .map_err(|_| {
                Error::new(&codes::SKILL_NOT_FOUND, format!("no skill '{name}'"))
                    .with_hint("prometheus journey list")
            })?;
        let read_only = || {
            Error::new(
                &codes::SKILL_READ_ONLY,
                format!(
                    "skill '{name}' is {} and read-only here",
                    skill.tier.as_str()
                ),
            )
            .with_hint(format!("prometheus skills disable {name}"))
        };
        match (skill.tier, skill.dir.path()) {
            _ if skill.name == ESSENTIAL_SKILL => Err(read_only()),
            (Tier::Profile, Some(dir)) => Ok(dir.to_path_buf()),
            _ => Err(read_only()),
        }
    }

    fn archive(&self, name: &str) -> Result<String, Error> {
        let dir = self.skill(name)?;
        let pinned = self.store.skill_usage(name)?.is_some_and(|u| u.pinned);
        if pinned {
            return Err(Error::new(
                &codes::SKILL_READ_ONLY,
                format!("skill '{name}' is pinned"),
            ));
        }
        let skills_dir = profile_skills(self.scope.dir());
        let io = |m: String| Error::new(&codes::SKILL_IO, m);
        let _lock = mutate::lock(&skills_dir, name).map_err(io)?;
        let archived = mutate::archive(&skills_dir, name, &dir, "").map_err(io)?;
        self.store.set_skill_state(name, SkillState::Archived)?;
        Ok(format!(
            "archived skill '{name}' to {} (move it back to restore it)",
            archived.display()
        ))
    }

    fn remove(&self, id: &str) -> Result<String, Error> {
        let entry = self.entry(id)?;
        let mut op = Op::new(Action::Remove);
        op.id = Some(entry.id.clone());
        self.commit(&entry, &[op])?;
        Ok(match self.last_revision(id)? {
            Some(rev) => {
                format!("removed {id} (revision {rev}; prometheus memory revert {rev} restores it)")
            }
            None => format!("removed {id}"),
        })
    }

    fn patch(&self, id: &str, edited: &str) -> Result<String, Error> {
        let entry = self.entry(id)?;
        let content = edited.trim();
        if content.is_empty() {
            return Err(Error::new(&codes::MEMORY_INVALID, "the entry is empty")
                .with_hint(format!("prometheus journey delete {id}")));
        }
        let redactor = self.scope.secrets().redactor()?;
        pmt_memory::scan::check(content, &redactor)
            .map_err(|m| Error::new(&codes::MEMORY_BLOCKED, m))?;
        let mut op = Op::new(Action::Patch);
        op.id = Some(entry.id.clone());
        op.content = Some(content.to_owned());
        self.commit(&entry, &[op])?;
        Ok(format!("updated {id} (from the next session)"))
    }

    fn commit(&self, entry: &MemoryEntry, ops: &[Op]) -> Result<(), Error> {
        let config = &self.scope.config().memory;
        let ctx = write::context(config, entry.target, MemoryOrigin::User, None);
        write::write(self.store, &ctx, ops, &[]).map(|_| ())
    }

    fn last_revision(&self, id: &str) -> Result<Option<i64>, Error> {
        let filter = RevisionFilter {
            origin: Some(MemoryOrigin::User),
            ..RevisionFilter::default()
        };
        Ok(self
            .store
            .memory_revisions(&filter)?
            .iter()
            .rev()
            .find(|r| r.entry_id == id)
            .map(|r| r.rev))
    }

    fn edit_skill(&self, name: &str, edited: &str) -> Result<String, Error> {
        self.skill(name)?;
        let mut setup = SessionSetup::new(self.scope.clone(), Arc::clone(self.store), "journey");
        setup.user_home = self.user_home.map(Path::to_path_buf);
        let session = SkillsSession::new(setup);
        let request = manage::Request::parse(&serde_json::json!({
            "action": "edit",
            "name": name,
            "content": edited,
        }))
        .map_err(|m| Error::new(&codes::SKILL_INVALID, m))?;
        // The user's own edit: applied now, never staged for approval.
        let outcome = manage::run(&session, &request, true)
            .map_err(|m| Error::new(&codes::SKILL_INVALID, m))?;
        Ok(outcome.message)
    }
}

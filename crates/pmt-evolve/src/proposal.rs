//! Proposals (spec: harness-evolution, Proposals and adoption policy;
//! Stale proposal protection). A successful selection becomes
//! `<profile>/evolve/proposals/<id>.toml` with the chosen tree beside it
//! (`<id>.tree/`). Evolution only proposes: a proposal becomes a harness
//! version only through adoption, which writes a new version with
//! `source = "evolve"` that applies from the next session. A proposal that
//! adds or changes scripts, hooks or subagents always waits for explicit
//! approval, and a proposal whose base is no longer the active version is
//! refused unless its diff rebases cleanly.

use std::path::{Path, PathBuf};

use pmt_config::harness::{Census, HarnessDir, Source, Tree, VersionInfo};
use pmt_config::schema::AdoptionPolicy;
use pmt_core::error::{Error, codes};
use serde::{Deserialize, Serialize};

use crate::harness::{Rules, diff, validate};

/// A proposal's status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    /// Waiting for the user.
    Pending,
    /// Written as a harness version.
    Adopted,
    /// Rejected by the user.
    Rejected,
    /// Another proposal on the same base was adopted.
    Superseded,
    /// The chosen tree equals its base.
    NoChange,
}

impl Status {
    /// The spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Adopted => "adopted",
            Self::Rejected => "rejected",
            Self::Superseded => "superseded",
            Self::NoChange => "no-change",
        }
    }
}

/// `<id>.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Proposal {
    /// `prop_<ULID>`.
    pub id: String,
    /// The job that produced it.
    pub job_id: String,
    /// `parallel` or `sequential`.
    pub mode: String,
    /// The chosen final's label.
    pub label: String,
    /// The harness version the job started from.
    pub base_version: String,
    /// The diff summary against the base.
    pub diff_summary: String,
    /// What the chosen tree holds.
    pub census: Census,
    /// The selector's reason.
    pub reason: String,
    /// `model`, `user` or `single`.
    pub selected_by: String,
    /// The distilled lesson (the experience library's).
    #[serde(default)]
    pub lesson: String,
    /// Its status.
    pub status: Status,
    /// It adds or changes scripts, hooks or subagents.
    pub approval_required: bool,
    /// RFC 3339.
    pub created_at: String,
    /// The version adoption wrote.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adopted_version: Option<String>,
}

/// What a job hands over to make a proposal.
#[derive(Debug, Clone)]
pub struct NewProposal<'a> {
    /// The job.
    pub job_id: &'a str,
    /// The mode.
    pub mode: &'a str,
    /// The chosen label.
    pub label: &'a str,
    /// The base version id.
    pub base_version: &'a str,
    /// Its tree.
    pub base: &'a Tree,
    /// The chosen tree.
    pub tree: &'a Tree,
    /// The selector's reason.
    pub reason: &'a str,
    /// Who chose.
    pub selected_by: &'a str,
}

/// An adoption.
#[derive(Debug, Clone)]
pub struct Adopted {
    /// The proposal, now `adopted`.
    pub proposal: Proposal,
    /// The version that was active.
    pub from: String,
    /// The new version (active).
    pub version: VersionInfo,
    /// Pending proposals marked `superseded`.
    pub superseded: Vec<String>,
}

/// Whether going from `base` to `tree` adds or changes a script, a skill
/// script, `hooks.toml` or a subagent definition.
#[must_use]
pub fn needs_approval(base: &Tree, tree: &Tree) -> bool {
    let summary = diff::summary(base, tree);
    summary
        .added
        .iter()
        .chain(&summary.modified)
        .any(|p| gated(p))
}

fn gated(path: &str) -> bool {
    path.starts_with("scripts/")
        || path.starts_with("agents/")
        || path == pmt_config::harness::HOOKS_FILE
        || (path.starts_with("skills/") && path.contains("/scripts/"))
}

/// `<profile>/evolve/proposals`.
#[derive(Debug, Clone)]
pub struct Proposals {
    root: PathBuf,
}

fn io(path: &Path, err: &std::io::Error) -> Error {
    Error::io(&codes::HARNESS_IO, path, err)
}

fn refused(message: impl Into<String>) -> Error {
    Error::new(&codes::EVOLVE_PROPOSAL_REFUSED, message)
}

impl Proposals {
    /// The proposals of the profile at `profile_dir`.
    #[must_use]
    pub fn of(profile_dir: &Path) -> Self {
        Self {
            root: profile_dir.join("evolve").join("proposals"),
        }
    }

    fn file(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.toml"))
    }

    fn tree_dir(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.tree"))
    }

    /// Writes a proposal (`pending`, or `no-change` when the tree equals
    /// its base).
    ///
    /// # Errors
    ///
    /// `harness.io`.
    pub fn create(&self, new: &NewProposal<'_>) -> Result<Proposal, Error> {
        let summary = diff::summary(new.base, new.tree);
        let proposal = Proposal {
            id: format!("prop_{}", ulid::Ulid::generate()),
            job_id: new.job_id.to_owned(),
            mode: new.mode.to_owned(),
            label: new.label.to_owned(),
            base_version: new.base_version.to_owned(),
            diff_summary: summary.to_string(),
            census: Census::of(new.tree),
            reason: new.reason.to_owned(),
            selected_by: new.selected_by.to_owned(),
            lesson: String::new(),
            status: if summary.is_empty() {
                Status::NoChange
            } else {
                Status::Pending
            },
            approval_required: needs_approval(new.base, new.tree),
            created_at: jiff::Timestamp::now().to_string(),
            adopted_version: None,
        };
        pmt_core::private_fs::create_dir(&self.root).map_err(|e| io(&self.root, &e))?;
        let dir = self.tree_dir(&proposal.id);
        new.tree.write(&dir).map_err(|e| io(&dir, &e))?;
        self.save(&proposal)?;
        Ok(proposal)
    }

    /// Writes `proposal` back.
    ///
    /// # Errors
    ///
    /// `harness.io`.
    pub fn save(&self, proposal: &Proposal) -> Result<(), Error> {
        let path = self.file(&proposal.id);
        let text =
            toml::to_string(proposal).map_err(|e| Error::new(&codes::HARNESS_IO, e.to_string()))?;
        pmt_core::private_fs::write_atomic(&path, text.as_bytes()).map_err(|e| io(&path, &e))
    }

    /// Proposal `id`.
    ///
    /// # Errors
    ///
    /// `evolve.proposal_not_found`; `harness.io`.
    pub fn get(&self, id: &str) -> Result<Proposal, Error> {
        let path = self.file(id);
        let text = std::fs::read_to_string(&path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => Error::new(
                &codes::EVOLVE_PROPOSAL_NOT_FOUND,
                format!("no harness proposal '{id}'"),
            ),
            _ => io(&path, &e),
        })?;
        toml::from_str(&text).map_err(|e| {
            Error::new(
                &codes::HARNESS_IO,
                format!("{}: {}", path.display(), e.message()),
            )
        })
    }

    /// The chosen tree of proposal `id`.
    ///
    /// # Errors
    ///
    /// `harness.io`.
    pub fn tree(&self, id: &str) -> Result<Tree, Error> {
        let dir = self.tree_dir(id);
        Tree::read(&dir).map(|(t, _)| t).map_err(|e| io(&dir, &e))
    }

    /// Every proposal, oldest first.
    ///
    /// # Errors
    ///
    /// `harness.io`.
    pub fn list(&self) -> Result<Vec<Proposal>, Error> {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return Ok(Vec::new());
        };
        let mut ids: Vec<String> = entries
            .filter_map(Result::ok)
            .filter_map(|e| {
                e.file_name()
                    .to_string_lossy()
                    .strip_suffix(".toml")
                    .map(str::to_owned)
            })
            .filter(|n| n.starts_with("prop_"))
            .collect();
        ids.sort();
        ids.iter().map(|id| self.get(id)).collect()
    }

    /// Rejects a pending proposal.
    ///
    /// # Errors
    ///
    /// `evolve.proposal_not_found`; `evolve.proposal_refused` when it is
    /// not pending.
    pub fn reject(&self, id: &str) -> Result<Proposal, Error> {
        let mut proposal = self.pending(id)?;
        proposal.status = Status::Rejected;
        self.save(&proposal)?;
        Ok(proposal)
    }

    fn pending(&self, id: &str) -> Result<Proposal, Error> {
        let proposal = self.get(id)?;
        if proposal.status != Status::Pending {
            return Err(refused(format!(
                "proposal {id} is {}, not pending",
                proposal.status.as_str()
            )));
        }
        Ok(proposal)
    }

    /// Adopts pending proposal `id`: a new active version with `source =
    /// "evolve"`; other pending proposals on the same base are superseded.
    ///
    /// # Errors
    ///
    /// `evolve.proposal_refused` (65) when it is not pending, its base is
    /// not the active version (without `rebase`), the rebase conflicts or
    /// the result does not validate.
    pub fn adopt(
        &self,
        profile_dir: &Path,
        id: &str,
        rebase: bool,
        rules: &Rules,
    ) -> Result<Adopted, Error> {
        let mut proposal = self.pending(id)?;
        let harness = HarnessDir::of(profile_dir);
        let active = harness.active_id()?;
        let chosen = self.tree(id)?;
        let mut tree = if active == proposal.base_version {
            chosen
        } else if rebase {
            let base = harness.tree(&proposal.base_version)?;
            let current = harness.tree(&active)?;
            rebased(&base, &chosen, &current).map_err(refused)?
        } else {
            return Err(refused(format!(
                "active harness changed since this proposal was produced (base {}, active {active}); re-run the job or adopt with --rebase",
                proposal.base_version
            )));
        };
        validate(&mut tree, rules).map_err(|errors| {
            refused(format!(
                "the proposed harness does not validate: {}",
                errors.join("; ")
            ))
        })?;
        let version = harness.write(&tree, Source::Evolve, Some(&proposal.job_id))?;
        harness.activate(&version.id)?;
        proposal.status = Status::Adopted;
        proposal.adopted_version = Some(version.id.clone());
        self.save(&proposal)?;
        let mut superseded = Vec::new();
        for mut other in self.list()? {
            if other.id != proposal.id
                && other.status == Status::Pending
                && other.base_version == proposal.base_version
            {
                other.status = Status::Superseded;
                self.save(&other)?;
                superseded.push(other.id);
            }
        }
        Ok(Adopted {
            proposal,
            from: active,
            version,
            superseded,
        })
    }
}

/// Applies `base` → `chosen` onto `current`, file by file: a path the
/// proposal changed must be unchanged in `current` since `base` (or
/// already equal to the proposal's).
///
/// # Errors
///
/// The conflicting paths.
pub fn rebased(base: &Tree, chosen: &Tree, current: &Tree) -> Result<Tree, String> {
    let summary = diff::summary(base, chosen);
    let mut out = current.clone();
    let mut conflicts = Vec::new();
    let changed = summary
        .added
        .iter()
        .chain(&summary.modified)
        .chain(&summary.removed);
    for path in changed {
        let theirs = current.get(path);
        let ours = chosen.get(path);
        if theirs != base.get(path) && theirs != ours {
            conflicts.push(path.clone());
            continue;
        }
        match ours {
            Some(text) => {
                out.files.insert(path.clone(), text.to_owned());
            }
            None => {
                out.files.remove(path);
            }
        }
    }
    if conflicts.is_empty() {
        Ok(out)
    } else {
        Err(format!(
            "the proposal does not rebase onto the active harness: conflicting paths {}",
            conflicts.join(", ")
        ))
    }
}

/// The adoption policy's verdict on a fresh proposal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Adopt now.
    Adopt,
    /// Wait for the user; `true` when approval is required by what it
    /// changes.
    Wait(bool),
    /// Nothing to adopt.
    Nothing,
}

/// What `policy` does with `proposal`.
#[must_use]
pub fn verdict(policy: AdoptionPolicy, proposal: &Proposal) -> Verdict {
    if proposal.status != Status::Pending {
        return Verdict::Nothing;
    }
    match policy {
        AdoptionPolicy::Auto if !proposal.approval_required => Verdict::Adopt,
        AdoptionPolicy::Auto => Verdict::Wait(true),
        AdoptionPolicy::Ask | AdoptionPolicy::Never => Verdict::Wait(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripts_hooks_and_subagents_need_approval_and_prose_does_not() {
        let base = Tree::builtin();
        let mut prose = base.clone();
        prose
            .files
            .insert("GUARDRAILS.md".into(), "Check totals.".into());
        assert!(!needs_approval(&base, &prose));
        for path in [
            "scripts/normalize.py",
            "hooks.toml",
            "agents/a/AGENT.md",
            "skills/s/scripts/x.sh",
        ] {
            let mut tree = base.clone();
            tree.files.insert(path.into(), "x".into());
            assert!(needs_approval(&base, &tree), "{path}");
        }
    }

    #[test]
    fn a_rebase_applies_only_without_conflicts() {
        let base = Tree::builtin();
        let mut chosen = base.clone();
        chosen.files.insert("GUARDRAILS.md".into(), "mine".into());
        let mut current = base.clone();
        current.files.insert("SYSTEM.md".into(), "theirs".into());
        let out = rebased(&base, &chosen, &current).unwrap();
        assert_eq!(out.get("GUARDRAILS.md"), Some("mine"));
        assert_eq!(out.get("SYSTEM.md"), Some("theirs"));
        current.files.insert("GUARDRAILS.md".into(), "other".into());
        assert!(
            rebased(&base, &chosen, &current)
                .unwrap_err()
                .contains("GUARDRAILS.md")
        );
    }
}

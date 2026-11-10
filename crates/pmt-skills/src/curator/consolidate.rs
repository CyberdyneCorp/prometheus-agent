//! Consolidation (`curator.consolidate`, `prometheus curator run
//! --consolidate`): a model review on the `auxiliary.curator` slot reads
//! the managed skills and proposes merges of overlapping ones into one
//! class-level skill. The curator checks each proposal (names it
//! manages, a valid `SKILL.md` naming the target, a scan that is not
//! `dangerous`), writes the target and archives the absorbed skills with
//! `absorbed_into` in their ledger entries; never deletes.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use pmt_core::error::{Error, codes};
use pmt_store::Store;
use pmt_store::skill_usage::{CreatedBy, SkillActivity};
use serde::{Deserialize, Serialize};

use super::Managed;
use super::moves;
use crate::frontmatter;
use crate::layout::SKILL_FILE;
use crate::ledger::{Actor, Evidence, Ledger};
use crate::scan::{Scanner, Verdict};

/// A skill the review may merge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Candidate {
    /// The skill.
    pub name: String,
    /// Its description.
    pub description: String,
    /// Its `SKILL.md`.
    pub skill_md: String,
}

/// One proposed merge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Merge {
    /// The skill that remains (new or one of the candidates).
    pub into: String,
    /// The skills merged into it (archived).
    pub absorbed: Vec<String>,
    /// The merged `SKILL.md`.
    pub skill_md: String,
}

/// The model review.
pub trait Consolidator {
    /// Proposes merges of `candidates`.
    ///
    /// # Errors
    ///
    /// The model call's failure.
    fn propose(&self, candidates: &[Candidate]) -> Result<Vec<Merge>, Error>;
}

/// The review's instructions.
pub const INSTRUCTIONS: &str = "You curate an agent's own skill library. The skills below were \
written by the agent. Merge skills that cover the same class of task into one class-level skill \
whose SKILL.md states the shared rules once (imperative, one clause of why), keeping topic depth \
short. Do not merge skills that are merely related. Never invent facts. Answer with JSON only: \
{\"merges\": [{\"into\": \"<name>\", \"absorbed\": [\"<name>\", ...], \"skill_md\": \"<complete \
SKILL.md with frontmatter name equal to into>\"}]}; answer {\"merges\": []} when nothing overlaps.";

/// The review prompt for `candidates`.
#[must_use]
pub fn prompt(candidates: &[Candidate]) -> String {
    let mut text = String::new();
    for c in candidates {
        let _ = write!(text, "=== {} ===\n{}\n\n", c.name, c.skill_md);
    }
    text
}

/// The merges of a review reply (the first JSON object in it).
///
/// # Errors
///
/// `skill.invalid` when no JSON object with `merges` is found.
pub fn parse(reply: &str) -> Result<Vec<Merge>, Error> {
    #[derive(Deserialize)]
    struct Reply {
        merges: Vec<Merge>,
    }
    let start = reply.find('{');
    let end = reply.rfind('}');
    let json = match (start, end) {
        (Some(s), Some(e)) if e > s => &reply[s..=e],
        _ => reply,
    };
    serde_json::from_str::<Reply>(json)
        .map(|r| r.merges)
        .map_err(|e| {
            Error::new(
                &codes::SKILL_INVALID,
                format!("the curator review answered no merges: {e}"),
            )
        })
}

/// What a merge did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Applied {
    /// The remaining skill.
    pub into: String,
    /// The archived ones.
    pub absorbed: Vec<String>,
}

/// The candidates of a pass: every managed skill still in place.
#[must_use]
pub fn candidates(managed: &[Managed]) -> Vec<Candidate> {
    managed
        .iter()
        .map(|m| Candidate {
            name: m.skill.name.clone(),
            description: m.skill.description.clone(),
            skill_md: m
                .skill
                .dir
                .read(SKILL_FILE)
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .unwrap_or_default(),
        })
        .collect()
}

/// Why a proposal is refused, if it is.
fn check(merge: &Merge, names: &BTreeSet<&str>, scanner: &dyn Scanner) -> Result<(), String> {
    if merge.absorbed.is_empty() {
        return Err("absorbs nothing".to_owned());
    }
    if let Some(bad) = merge
        .absorbed
        .iter()
        .find(|a| !names.contains(a.as_str()) || **a == merge.into)
    {
        return Err(format!(
            "'{bad}' is not a curator-managed skill it may absorb"
        ));
    }
    let doc = frontmatter::parse(&merge.skill_md)
        .map_err(|e| format!("invalid SKILL.md: {}", e.reason))?;
    if doc.name != merge.into {
        return Err(format!(
            "its SKILL.md names '{}', not '{}'",
            doc.name, merge.into
        ));
    }
    match scanner.scan(&[(SKILL_FILE.to_owned(), merge.skill_md.clone().into_bytes())]) {
        Ok(report) if report.verdict == Verdict::Dangerous => {
            Err(format!("security scan: {}", report.summary()))
        }
        Ok(_) => Ok(()),
        Err(e) => Err(format!("security scan failed: {e}")),
    }
}

/// Applies the valid `merges`; refused ones are reported in `errors`.
pub fn apply(
    ledger: &Ledger<'_>,
    store: &Store,
    managed: &[Managed],
    merges: &[Merge],
    scanner: &dyn Scanner,
    errors: &mut Vec<String>,
) -> Vec<Applied> {
    let mut names: BTreeSet<&str> = managed.iter().map(|m| m.skill.name.as_str()).collect();
    let mut applied = Vec::new();
    for merge in merges {
        if let Err(why) = check(merge, &names, scanner) {
            errors.push(format!("merge into '{}' refused: {why}", merge.into));
            continue;
        }
        match apply_one(ledger, store, managed, merge) {
            Ok(()) => {
                for a in &merge.absorbed {
                    names.remove(a.as_str());
                }
                applied.push(Applied {
                    into: merge.into.clone(),
                    absorbed: merge.absorbed.clone(),
                });
            }
            Err(e) => errors.push(format!(
                "merge into '{}' failed: {}",
                merge.into,
                e.message()
            )),
        }
    }
    applied
}

fn apply_one(
    ledger: &Ledger<'_>,
    store: &Store,
    managed: &[Managed],
    merge: &Merge,
) -> Result<(), Error> {
    let existing = managed.iter().find(|m| m.skill.name == merge.into);
    let rel = existing.map_or_else(|| merge.into.clone(), |m| m.rel.clone());
    let file_rel = format!("{rel}/{SKILL_FILE}");
    let before = ledger.capture([file_rel.clone()]);
    let path = ledger.skills_dir().join(&rel).join(SKILL_FILE);
    if let Some(parent) = path.parent() {
        pmt_core::private_fs::create_dir(parent)
            .map_err(|e| Error::io(&codes::SKILL_IO, parent, &e))?;
    }
    pmt_core::private_fs::write_atomic(&path, merge.skill_md.as_bytes())
        .map_err(|e| Error::io(&codes::SKILL_IO, &path, &e))?;
    let action = if existing.is_some() { "edit" } else { "create" };
    ledger.record(
        Actor::Curator,
        action,
        &merge.into,
        &before,
        &Evidence::default(),
    );
    if let Err(e) =
        store.record_skill_activity(&merge.into, SkillActivity::Patched, CreatedBy::Agent)
    {
        tracing::warn!(skill = %merge.into, error = %e, "skill usage not recorded");
    }
    for name in &merge.absorbed {
        let Some(m) = managed.iter().find(|m| &m.skill.name == name) else {
            continue;
        };
        let evidence = Evidence {
            absorbed_into: Some(merge.into.clone()),
            session_id: None,
        };
        moves::archive(ledger, store, name, &m.rel, Actor::Curator, &evidence)?;
    }
    Ok(())
}

//! Choosing one final per mode (spec: harness-evolution, Blind
//! anonymised selection; User-driven selection). The finals that ended ok
//! with a deliverable get bijective base-26 ids (`A`, `B`, ... `AA`) by
//! shuffling their sorted labels with a PRNG seeded from
//! SHA-256(`<job>|<k>|<mode>`), so the same job always maps a label to the
//! same id. The selector sees only the task and `candidates/<ID>.md`; a
//! user sees the same view through `evolve compare` and answers with
//! `evolve choose`.

use std::fmt::Write as _;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::evidence;
use super::ledger::{Entry, Kind};
use super::modes::Candidate;
use super::ports::SessionKind;
use super::prompts::{self, SELECTOR_SYSTEM};
use super::run::{Run, SessionCall, SessionError};
use super::stage::truncate;
use super::summary::Selected;

/// The longest selector reason kept.
pub const MAX_WHY_CHARS: usize = 2000;

/// The anonymous id of position `n` (`0` → `A`, `25` → `Z`, `26` → `AA`).
#[must_use]
pub fn anonymous_id(mut n: usize) -> String {
    let mut id = Vec::new();
    loop {
        let digit = u8::try_from(n % 26).unwrap_or(0);
        id.push(char::from(b'A' + digit));
        if n < 26 {
            break;
        }
        n = n / 26 - 1;
    }
    id.iter().rev().collect()
}

/// `label` → id for `labels`, stable for (`job_id`, `k`, `mode`).
#[must_use]
pub fn anonymous_ids(job_id: &str, k: u8, mode: &str, labels: &[String]) -> Vec<(String, String)> {
    let digest = Sha256::digest(format!("{job_id}|{k}|{mode}").as_bytes());
    let hex: String = digest[..6].iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    });
    let mut state = u64::from_str_radix(&hex, 16).unwrap_or(0);
    let mut order = labels.to_vec();
    order.sort();
    for i in (1..order.len()).rev() {
        let j = usize::try_from(splitmix(&mut state) % (i as u64 + 1)).unwrap_or(0);
        order.swap(i, j);
    }
    order
        .into_iter()
        .enumerate()
        .map(|(n, label)| (label, anonymous_id(n)))
        .collect()
}

fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// The id of a ```` ```choice ```` block and the `<why>` of `reply`.
#[must_use]
pub fn parse_choice(reply: &str) -> Option<(String, String)> {
    let start = reply.find("```choice")? + "```choice".len();
    let rest = &reply[start..];
    let block = &rest[..rest.find("```")?];
    let id = block
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())?
        .to_owned();
    let why = reply
        .find("<why>")
        .and_then(|a| {
            let tail = &reply[a + "<why>".len()..];
            tail.find("</why>").map(|b| tail[..b].trim().to_owned())
        })
        .unwrap_or_default();
    Some((id, truncate(&why, MAX_WHY_CHARS)))
}

/// `modes/<mode>/choice.toml`: the user's pick.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserChoice {
    /// The anonymous id.
    pub id: String,
}

/// A candidate's anonymous view: its deliverable with character and
/// line counts, and nothing that names its harness, label or cost.
#[must_use]
pub fn candidate_view(id: &str, outcome: &super::ports::RolloutOutcome) -> String {
    let produced = evidence::produced(outcome);
    format!(
        "# Candidate {id}\n\n{} characters, {} lines\n\n{produced}",
        produced.chars().count(),
        produced.lines().count()
    )
}

/// How a mode's selection ended.
#[derive(Debug, Clone)]
pub(crate) enum Selection {
    /// Index into the finals, and the record.
    Chosen(usize, Selected),
    /// `evolve.selection = "user"` and no choice yet.
    Awaiting,
    /// `selection failed`.
    Failed(String),
}

impl Run<'_> {
    fn mode_dir(&self, mode: &str) -> std::path::PathBuf {
        self.dir.join("modes").join(mode)
    }

    /// The anonymous ids of the eligible finals.
    pub(crate) fn eligible(&self, mode: &str, finals: &[Candidate]) -> Vec<(String, String)> {
        let labels: Vec<String> = finals
            .iter()
            .filter(|c| c.rollout.outcome.ok && c.rollout.outcome.delivered())
            .map(|c| c.stage.label.clone())
            .collect();
        anonymous_ids(&self.job.id, self.job.k, mode, &labels)
    }

    /// Selects one final of `mode`; the selector's ledger entry is
    /// returned with it.
    pub(crate) async fn select(
        &self,
        mode: &str,
        finals: &[Candidate],
    ) -> (Selection, Option<Entry>) {
        let ids = self.eligible(mode, finals);
        let index = |label: &str| finals.iter().position(|c| c.stage.label == label);
        if ids.is_empty() {
            return (
                Selection::Failed("no final is ok with a deliverable".into()),
                None,
            );
        }
        if let Err(err) = self.write_candidates(mode, finals, &ids) {
            return (Selection::Failed(err), None);
        }
        if self.job.k == 1 && ids.len() == 1 {
            let (label, id) = &ids[0];
            let selected = chosen(label, id, "single", "");
            return (
                index(label).map_or(Selection::Awaiting, |i| Selection::Chosen(i, selected)),
                None,
            );
        }
        if self.engine.config.selection == pmt_config::schema::EvolveSelection::User {
            return (self.user_choice(mode, &ids, finals), None);
        }
        let (outcome, entry) = self.selector(mode, &ids).await;
        let selection = match outcome {
            Ok((id, why)) => match ids.iter().find(|(_, i)| *i == id) {
                Some((label, _)) => index(label).map_or_else(
                    || Selection::Failed(format!("selection failed: unknown id {id}")),
                    |i| Selection::Chosen(i, chosen(label, &id, "model", &why)),
                ),
                None => Selection::Failed(format!("selection failed: unknown id {id}")),
            },
            Err(why) => Selection::Failed(format!("selection failed: {why}")),
        };
        (selection, entry)
    }

    fn user_choice(&self, mode: &str, ids: &[(String, String)], finals: &[Candidate]) -> Selection {
        let path = self.mode_dir(mode).join("choice.toml");
        let Some(choice) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| toml::from_str::<UserChoice>(&t).ok())
        else {
            return Selection::Awaiting;
        };
        let found = ids
            .iter()
            .find(|(_, id)| *id == choice.id)
            .and_then(|(label, id)| {
                finals
                    .iter()
                    .position(|c| c.stage.label == *label)
                    .map(|i| Selection::Chosen(i, chosen(label, id, "user", "")))
            });
        found.unwrap_or_else(|| {
            Selection::Failed(format!("selection failed: unknown id {}", choice.id))
        })
    }

    /// `modes/<mode>/candidates/<ID>.md` (what `evolve compare` shows).
    fn write_candidates(
        &self,
        mode: &str,
        finals: &[Candidate],
        ids: &[(String, String)],
    ) -> Result<(), String> {
        let dir = self.mode_dir(mode).join("candidates");
        let _ = std::fs::remove_dir_all(&dir);
        pmt_core::private_fs::create_dir(&dir).map_err(|e| e.to_string())?;
        for (label, id) in ids {
            if let Some(c) = finals.iter().find(|c| c.stage.label == *label) {
                std::fs::write(
                    dir.join(format!("{id}.md")),
                    candidate_view(id, &c.rollout.outcome),
                )
                .map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }

    async fn selector(
        &self,
        mode: &str,
        ids: &[(String, String)],
    ) -> (Result<(String, String), String>, Option<Entry>) {
        let label = format!("select-{mode}");
        let workspace = match self.scratch(&label) {
            Ok(dir) => dir,
            Err(err) => return (Err(err.to_string()), None),
        };
        let result = self.selector_in(&label, mode, ids, &workspace).await;
        let _ = std::fs::remove_dir_all(&workspace);
        result
    }

    async fn selector_in(
        &self,
        label: &str,
        mode: &str,
        ids: &[(String, String)],
        workspace: &Path,
    ) -> (Result<(String, String), String>, Option<Entry>) {
        let context = evidence::context(workspace);
        let task = self.dir.join("task");
        let built = pmt_core::private_fs::copy_tree(
            &self.mode_dir(mode).join("candidates"),
            &context.join("candidates"),
        )
        .and_then(|()| std::fs::copy(task.join("task.md"), context.join("task.md")).map(|_| ()))
        .and_then(|()| {
            if task.join("source").is_dir() {
                pmt_core::private_fs::copy_tree(&task.join("source"), &context.join("source"))
            } else {
                Ok(())
            }
        });
        if let Err(err) = built {
            return (Err(err.to_string()), None);
        }
        if let Err(hit) = self.tripwire.check_dir(&context) {
            tracing::warn!(job = %self.job.id, stage = label, "{hit}");
            return (Err(hit), None);
        }
        let id_list: Vec<String> = ids.iter().map(|(_, id)| id.clone()).collect();
        let mission = prompts::selection(&id_list);
        let call = SessionCall {
            label,
            kind: SessionKind::Select,
            system: SELECTOR_SYSTEM,
            mission: &mission,
            workspace,
        };
        let entry = |usd: f64, cached: bool| Entry {
            kind: Kind::Selection,
            label: label.to_owned(),
            usd,
            cached,
            attempts: 1,
        };
        match self.session(call).await {
            Ok(done) => {
                let parsed = parse_choice(&done.text)
                    .ok_or_else(|| "no choice block in the reply".to_owned());
                (parsed, Some(entry(done.usd, done.cached)))
            }
            Err(SessionError::Failed { message, usd }) => (Err(message), Some(entry(usd, false))),
            Err(SessionError::Stopped(stop)) => (Err(stop.status().to_owned()), None),
        }
    }
}

fn chosen(label: &str, id: &str, by: &str, why: &str) -> Selected {
    Selected {
        label: label.to_owned(),
        id: id.to_owned(),
        selected_by: by.to_owned(),
        reason: why.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_bijective_base_26() {
        assert_eq!(anonymous_id(0), "A");
        assert_eq!(anonymous_id(25), "Z");
        assert_eq!(anonymous_id(26), "AA");
        assert_eq!(anonymous_id(27), "AB");
        assert_eq!(anonymous_id(701), "ZZ");
        assert_eq!(anonymous_id(702), "AAA");
    }

    /// Scenario: Stable ids.
    #[test]
    fn the_same_job_k_and_mode_give_every_label_the_same_id() {
        let labels: Vec<String> = ["agg1", "agg2", "agg3"].map(String::from).to_vec();
        let first = anonymous_ids("evo_1", 3, "parallel", &labels);
        let mut reversed = labels.clone();
        reversed.reverse();
        assert_eq!(first, anonymous_ids("evo_1", 3, "parallel", &reversed));
        let ids: BTreeSet<_> = first.iter().map(|(_, id)| id.as_str()).collect();
        assert_eq!(ids, ["A", "B", "C"].into_iter().collect());
    }

    #[test]
    fn a_choice_block_names_the_id_and_the_reason() {
        let reply = "Looking...\n```choice\n B \n```\n<why>it sums every line</why>";
        assert_eq!(
            parse_choice(reply),
            Some(("B".into(), "it sums every line".into()))
        );
        assert_eq!(parse_choice("I pick B"), None);
    }

    use std::collections::BTreeSet;
}

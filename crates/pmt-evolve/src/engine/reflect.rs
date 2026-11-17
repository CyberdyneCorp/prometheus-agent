//! Reflection (spec: experience-library, Reflection workspace; Experience
//! distillation). After a mode's selection chose a final, the whole
//! search is laid out in `<job>/reflection/<mode><k>/` — every rollout,
//! the chosen and base harnesses, the pool of non-base candidates, every
//! decision note and `adoption.json` — and screened by the leak globs
//! and the verdict tripwire before one reflector session (label
//! `experience.<mode><k>`) distils at most one lesson. Any failure yields
//! no lesson; its cost stays charged to the job.

use std::path::Path;

use serde_json::json;

use super::evidence::{self, PoolItem};
use super::ledger::{Entry, Kind};
use super::modes::Candidate;
use super::ports::SessionKind;
use super::prompts::{self, REFLECTOR_SYSTEM};
use super::run::{RolloutDone, Run, SessionCall, SessionError};
use super::summary::Selected;
use crate::experience::lesson::{self, Distilled};

/// What a mode hands to its reflection.
pub(crate) struct Search<'a> {
    pub mode: &'a str,
    pub base: &'a [RolloutDone],
    /// Every non-base candidate of the mode (stages and finals).
    pub candidates: &'a [Candidate],
    pub chosen: &'a Candidate,
    pub selected: &'a Selected,
    pub ids: &'a [(String, String)],
}

impl Run<'_> {
    /// Reflects on `search`; the distilled edits and lesson (none on any
    /// failure) and the reflection's ledger entry.
    pub(crate) async fn reflect(&self, search: &Search<'_>) -> (Distilled, Option<Entry>) {
        let label = format!("experience.{}{}", search.mode, self.job.k);
        let dir = self
            .dir
            .join("reflection")
            .join(format!("{}{}", search.mode, self.job.k));
        let distilled = match self.reflection_workspace(&dir, search) {
            Ok(()) => self.distil(&label, &dir).await,
            Err(why) => {
                tracing::warn!(job = %self.job.id, mode = search.mode, "no reflection: {why}");
                (Distilled::default(), None)
            }
        };
        let confidence = distilled
            .0
            .lesson
            .as_ref()
            .map_or("none", |l| l.confidence.as_str());
        self.log.emit(
            "experience.lesson.distilled",
            json!({"job": self.job.id, "mode": search.mode, "confidence": confidence}),
        );
        distilled
    }

    fn reflection_workspace(&self, dir: &Path, search: &Search<'_>) -> Result<(), String> {
        let io = |e: std::io::Error| format!("reflection workspace: {e}");
        if dir.exists() {
            std::fs::remove_dir_all(dir).map_err(io)?;
        }
        pmt_core::private_fs::create_dir(dir).map_err(io)?;
        std::fs::copy(self.dir.join("task").join("task.md"), dir.join("task.md")).map_err(io)?;
        let trunc = self.engine.config.evidence.obs_trunc_chars.max(10_000);
        let rollouts = search.base.iter().map(|b| (&b.label, &b.outcome)).chain(
            search
                .candidates
                .iter()
                .map(|c| (&c.stage.label, &c.rollout.outcome)),
        );
        let rollouts_dir = dir.join("rollouts");
        pmt_core::private_fs::create_dir(&rollouts_dir).map_err(io)?;
        for (label, outcome) in rollouts {
            let text = evidence::render(label, outcome, trunc);
            std::fs::write(rollouts_dir.join(format!("{label}.md")), text).map_err(io)?;
        }
        search
            .chosen
            .stage
            .tree
            .write(&dir.join("harness"))
            .map_err(io)?;
        self.active.write(&dir.join("base_harness")).map_err(io)?;
        let decisions = dir.join("decisions");
        pmt_core::private_fs::create_dir(&decisions).map_err(io)?;
        for c in search.candidates {
            let item = PoolItem {
                label: &c.stage.label,
                tree: &c.stage.tree,
                diff_base: &c.base,
                rollout: &c.rollout.outcome,
                decision: &c.stage.decision,
            };
            evidence::pool_item(dir, &item, trunc)?;
            std::fs::write(
                decisions.join(format!("{}.md", c.stage.label)),
                &c.stage.decision,
            )
            .map_err(io)?;
        }
        let ids: Vec<&str> = search.ids.iter().map(|(_, id)| id.as_str()).collect();
        let adoption = json!({
            "chosen_label": search.selected.label,
            "candidate_ids": ids,
            "reason": search.selected.reason,
        });
        let text = serde_json::to_string_pretty(&adoption).map_err(|e| e.to_string())?;
        std::fs::write(dir.join("adoption.json"), text).map_err(io)?;
        if let Err(hit) = self.tripwire.check_dir(dir) {
            tracing::warn!(job = %self.job.id, stage = "reflection", "{hit}");
            return Err(hit);
        }
        Ok(())
    }

    async fn distil(&self, label: &str, dir: &Path) -> (Distilled, Option<Entry>) {
        let workspace = match self.scratch(label) {
            Ok(w) => w,
            Err(err) => return (Distilled::default(), self.failed(label, &err.to_string())),
        };
        if let Err(err) = pmt_core::private_fs::copy_tree(dir, &evidence::context(&workspace)) {
            let _ = std::fs::remove_dir_all(&workspace);
            return (Distilled::default(), self.failed(label, &err.to_string()));
        }
        let mission = prompts::reflection(label, &self.job.domain);
        let call = SessionCall {
            label,
            kind: SessionKind::Reflect,
            system: REFLECTOR_SYSTEM,
            mission: &mission,
            workspace: &workspace,
        };
        let outcome = self.session(call).await;
        let _ = std::fs::remove_dir_all(&workspace);
        let entry = |usd: f64, cached: bool| Entry {
            kind: Kind::Reflection,
            label: label.to_owned(),
            usd,
            cached,
            attempts: 1,
        };
        match outcome {
            Ok(done) => (
                lesson::parse(&done.text),
                Some(entry(done.usd, done.cached)),
            ),
            Err(SessionError::Failed { message, usd }) => {
                tracing::warn!(job = %self.job.id, label, "reflection failed: {message}");
                (Distilled::default(), Some(entry(usd, false)))
            }
            Err(SessionError::Stopped(_)) => (Distilled::default(), None),
        }
    }

    fn failed(&self, label: &str, why: &str) -> Option<Entry> {
        tracing::warn!(job = %self.job.id, label, "reflection failed: {why}");
        None
    }
}

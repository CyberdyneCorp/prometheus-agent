//! `summary.json` (spec: harness-evolution, Job lifecycle, events and
//! RPC): the job's status, stages, selections, proposals and spend,
//! atomically replaced as the job advances, so `evolve status`, the RPC
//! and the dashboard read a consistent view at any moment.

use std::path::Path;

use pmt_core::error::{Error, codes};
use serde::{Deserialize, Serialize};

use super::ledger::Costs;

/// The statuses of a job.
pub const STATUSES: &[&str] = &[
    "queued",
    "running",
    "awaiting-selection",
    "cancelled",
    "base-failed",
    "budget-exceeded",
    "partial",
    "failed",
    "ok",
];

/// One base rollout.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BaseSummary {
    /// `base<i>`.
    pub label: String,
    /// It ended with a final message.
    pub ok: bool,
    /// Why not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One stage, composition or checkpoint.
#[allow(clippy::struct_excessive_bools)] // independent facts of one stage
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StageSummary {
    /// Its label.
    pub label: String,
    /// It inherited its fallback tree.
    pub fell_back: bool,
    /// Attempts it took.
    pub attempts: u32,
    /// The accepted tree equals its fallback.
    pub kept_base: bool,
    /// Its sessions' spend.
    pub usd: f64,
    /// Served from the cache.
    pub cached: bool,
    /// Why it fell back (at most 400 characters).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Its rollout ended ok.
    pub rollout_ok: bool,
}

/// Similarity of a parallel mode's finals.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Similarity {
    /// Finals.
    pub n: usize,
    /// Distinct finals.
    pub n_distinct: usize,
    /// Mean pairwise Jaccard similarity of their line sets.
    pub mean_line_jaccard: f64,
}

/// The selection of a mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selected {
    /// The chosen final's label.
    pub label: String,
    /// Its anonymous id.
    pub id: String,
    /// `model`, `user` or `single` (k = 1).
    pub selected_by: String,
    /// The selector's reason (at most 2000 characters).
    pub reason: String,
}

/// One mode.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModeSummary {
    /// `parallel` or `sequential`.
    pub mode: String,
    /// `running`, `ok`, `selection failed`, `awaiting-selection`,
    /// `stopped` or an identity violation.
    pub status: String,
    /// Its stages in serial order.
    pub stages: Vec<StageSummary>,
    /// Parallel mode: how alike the finals are.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub similarity: Option<Similarity>,
    /// The anonymous ids of the finals (`label` → `id`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<(String, String)>,
    /// The selection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected: Option<Selected>,
    /// The proposal it produced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposal_id: Option<String>,
}

/// `summary.json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct JobSummary {
    /// The job.
    pub job_id: String,
    /// One of [`STATUSES`].
    pub status: String,
    /// Why it failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The harness version the job started from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_version: Option<String>,
    /// The base rollouts.
    #[serde(default)]
    pub base: Vec<BaseSummary>,
    /// The modes.
    #[serde(default)]
    pub modes: Vec<ModeSummary>,
    /// Spend by kind.
    #[serde(default)]
    pub costs: Costs,
    /// The estimate printed before the job started.
    #[serde(default)]
    pub estimate_usd: f64,
    /// Model calls this run made (cache hits excluded).
    #[serde(default)]
    pub model_calls: u32,
    /// Rollouts this run executed (cache hits excluded).
    #[serde(default)]
    pub rollouts_run: u32,
    /// RFC 3339.
    #[serde(default)]
    pub updated_at: String,
}

impl JobSummary {
    /// Reads `<job>/summary.json`; `None` before the job first ran.
    #[must_use]
    pub fn read(dir: &Path) -> Option<Self> {
        serde_json::from_str(&std::fs::read_to_string(dir.join("summary.json")).ok()?).ok()
    }

    /// Atomically replaces `<job>/summary.json`.
    ///
    /// # Errors
    ///
    /// `harness.io`.
    pub fn write(&mut self, dir: &Path) -> Result<(), Error> {
        self.updated_at = jiff::Timestamp::now().to_string();
        let path = dir.join("summary.json");
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|e| Error::new(&codes::HARNESS_IO, e.to_string()))?;
        pmt_core::private_fs::write_atomic(&path, &bytes)
            .map_err(|e| Error::io(&codes::HARNESS_IO, &path, &e))
    }

    /// The summary of mode `mode`, created when missing.
    ///
    /// # Panics
    ///
    /// Never: the entry is pushed just before it is returned.
    pub fn mode(&mut self, mode: &str) -> &mut ModeSummary {
        if let Some(i) = self.modes.iter().position(|m| m.mode == mode) {
            return &mut self.modes[i];
        }
        self.modes.push(ModeSummary {
            mode: mode.to_owned(),
            status: "running".to_owned(),
            stages: Vec::new(),
            similarity: None,
            candidates: Vec::new(),
            selected: None,
            proposal_id: None,
        });
        self.modes.last_mut().expect("just pushed")
    }
}

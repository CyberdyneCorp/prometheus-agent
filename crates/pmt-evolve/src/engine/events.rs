//! A job's trajectory events (spec: harness-evolution, Job lifecycle,
//! events and RPC): `evolve.job.started`, `evolve.stage.finished`,
//! `evolve.rollout.finished`, `evolve.selection.finished`,
//! `evolve.proposal.ready`, `evolve.proposal.adopted` and
//! `evolve.job.finished`, in the trajectory grammar's line shape
//! (`ts`, `type`, `payload`), appended to `<job>/events.jsonl`. A job is
//! not a session, so its events live with the job.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// `<job>/events.jsonl`.
#[derive(Debug, Clone)]
pub struct JobLog {
    path: PathBuf,
    job_id: String,
}

impl JobLog {
    /// The log of the job in `dir`.
    #[must_use]
    pub fn new(dir: &Path, job_id: &str) -> Self {
        Self {
            path: dir.join("events.jsonl"),
            job_id: job_id.to_owned(),
        }
    }

    /// Appends one event; a failure is logged, never fatal.
    pub fn emit(&self, kind: &str, mut payload: Value) {
        payload["job_id"] = json!(self.job_id);
        let line = json!({
            "ts": jiff::Timestamp::now().to_string(),
            "type": kind,
            "payload": payload,
        });
        tracing::info!(job = %self.job_id, event = kind, "{}", line["payload"]);
        let appended = pmt_core::private_fs::append_options()
            .open(&self.path)
            .and_then(|mut file| {
                use std::io::Write as _;
                writeln!(file, "{line}")
            });
        if let Err(err) = appended {
            tracing::warn!(path = %self.path.display(), error = %err, "evolution event not recorded");
        }
    }

    /// `evolve.stage.finished`.
    pub fn stage(&self, stage: &str, status: &str) {
        self.emit(
            "evolve.stage.finished",
            json!({"stage": stage, "status": status}),
        );
    }

    /// `evolve.rollout.finished`.
    pub fn rollout(&self, candidate: &str, ok: bool) {
        self.emit(
            "evolve.rollout.finished",
            json!({"candidate": candidate, "task": if ok {"ok"} else {"failed"}, "score": null}),
        );
    }

    /// Every event recorded so far.
    #[must_use]
    pub fn read(dir: &Path) -> Vec<Value> {
        std::fs::read_to_string(dir.join("events.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }
}

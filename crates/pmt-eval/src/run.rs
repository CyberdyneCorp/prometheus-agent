//! The run layout (spec: replay-and-evaluation, Eval run; Eval runs are
//! resumable): `<profile>/evals/runs/<run_id>/` holds `run.json` and, per
//! task and repeat, `tasks/<task_id>/<repeat>/` with `trajectory.jsonl`,
//! `deliverables/` (`answer.md` and the workspace's `files/`),
//! `result.json` (no score) and the completion marker `done.json`, whose
//! fingerprint of the deliverables decides whether a resumed run skips
//! the task.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use pmt_core::error::{Error, codes};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// `run.json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RunInfo {
    /// The run id.
    pub run_id: String,
    /// `suite`, `bench` or `batch`.
    pub kind: String,
    /// The suite's (or adapter's, or dataset's) name.
    pub suite: String,
    /// Its version.
    pub suite_version: String,
    /// Where the suite (or dataset) is.
    pub suite_path: PathBuf,
    /// The provider the tasks ran on.
    pub provider: String,
    /// The model.
    pub model: String,
    /// The harness version asked for (`--harness`, else the active one).
    pub harness_ref: String,
    /// The harness tree's fingerprint.
    pub harness_hash: String,
    /// `prometheus <version> (<commit>)`.
    pub prometheus_build: String,
    /// `off`, `read` or `read_write`.
    pub cache: String,
    /// Repeats per task.
    pub repeat: u32,
    /// The sampling seed.
    pub seed: u64,
    /// The run's spend cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_usd: Option<f64>,
    /// The toolset distribution, when one sampled the toolsets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub toolset_distribution: Option<String>,
    /// Each task's toolsets (sampled or given).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub toolsets: BTreeMap<String, Vec<String>>,
    /// When the run (first) started.
    pub started_at: String,
    /// When it last finished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
}

impl RunInfo {
    /// Refuses to resume `self` (the recorded run) with `new`.
    ///
    /// # Errors
    ///
    /// `eval.run_mismatch` (2) naming the first difference.
    pub fn check_resume(&self, new: &Self) -> Result<(), Error> {
        let model = |i: &Self| format!("{}/{}", i.provider, i.model);
        let pairs = [
            (
                "suite version",
                self.suite_version.clone(),
                new.suite_version.clone(),
            ),
            ("model", model(self), model(new)),
            (
                "harness hash",
                self.harness_hash.clone(),
                new.harness_hash.clone(),
            ),
        ];
        match pairs.into_iter().find(|(_, old, new)| old != new) {
            Some((what, old, now)) => Err(Error::new(
                &codes::EVAL_RUN_MISMATCH,
                format!(
                    "run {} was started with {what} {old}; this invocation has {now}",
                    self.run_id
                ),
            )
            .with_hint("resume with the same settings, or pass another --run-id")),
            None => Ok(()),
        }
    }
}

/// `result.json` of one task and repeat (never a score).
#[allow(clippy::struct_excessive_bools)] // the file's flat schema
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TaskResult {
    /// The task.
    pub task_id: String,
    /// The repeat (1-based).
    pub repeat: u32,
    /// The task executed to a final answer.
    pub ok: bool,
    /// It produced deliverables.
    pub delivered: bool,
    /// Its spend.
    pub cost_usd: f64,
    /// Its wall time.
    pub wall_secs: f64,
    /// Why it did not execute.
    pub error: Option<String>,
    /// `budget` when the run's budget stopped it before it started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped: Option<String>,
    /// The exit status a one-shot run of the task would have had.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_status: Option<u8>,
    /// The budget refused one of its model calls.
    #[serde(default)]
    pub budget_stopped: bool,
    /// The task session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Its toolsets.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub toolsets: Vec<String>,
    /// It touched grader material (excluded from scoring).
    #[serde(default)]
    pub contaminated: bool,
    /// What it touched.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contamination: Vec<String>,
    /// Input tokens.
    #[serde(default)]
    pub input_tokens: u64,
    /// Output tokens.
    #[serde(default)]
    pub output_tokens: u64,
    /// Model calls without a price.
    #[serde(default)]
    pub unpriced_calls: u64,
}

impl TaskResult {
    /// What harness evolution and selection may see of a run result:
    /// `ok`, `delivered`, `cost_usd`, `wall_secs` and `error` only.
    #[must_use]
    pub fn for_evolution(&self) -> serde_json::Value {
        serde_json::json!({
            "ok": self.ok,
            "delivered": self.delivered,
            "cost_usd": self.cost_usd,
            "wall_secs": self.wall_secs,
            "error": self.error,
        })
    }

    /// The task executed (it can be judged).
    #[must_use]
    pub fn executed(&self) -> bool {
        self.skipped.is_none() && self.delivered
    }
}

/// `done.json`: the completion marker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Done {
    /// [`fingerprint`] of `deliverables/` at completion.
    pub fingerprint: String,
    /// When.
    pub finished_at: String,
}

/// What a resumed run does with one task and repeat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resume {
    /// Never ran: run it.
    Run,
    /// Completed and untouched: skip it.
    Skip,
    /// Run it again, after deleting its outputs, for this reason.
    Rerun(String),
}

/// One run directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunDir {
    /// `<profile>/evals/runs/<run_id>`.
    pub dir: PathBuf,
}

/// The runs directory of a profile.
#[must_use]
pub fn runs_dir(profile_dir: &Path) -> PathBuf {
    profile_dir.join("evals").join("runs")
}

impl RunDir {
    /// Run `run_id` of the profile at `profile_dir`.
    #[must_use]
    pub fn of(profile_dir: &Path, run_id: &str) -> Self {
        Self {
            dir: runs_dir(profile_dir).join(run_id),
        }
    }

    /// An existing run.
    ///
    /// # Errors
    ///
    /// `eval.run_not_found` (66).
    pub fn existing(profile_dir: &Path, run_id: &str) -> Result<Self, Error> {
        let run = Self::of(profile_dir, run_id);
        if run.info().is_none() {
            return Err(Error::new(
                &codes::EVAL_RUN_NOT_FOUND,
                format!("no eval run '{run_id}' in {}", run.dir.display()),
            ));
        }
        Ok(run)
    }

    /// `run.json`, when the run exists.
    #[must_use]
    pub fn info(&self) -> Option<RunInfo> {
        read_json(&self.dir.join("run.json"))
    }

    /// Writes `run.json`.
    ///
    /// # Errors
    ///
    /// `eval.io`.
    pub fn write_info(&self, info: &RunInfo) -> Result<(), Error> {
        write_json(&self.dir.join("run.json"), info)
    }

    /// The directory of `task`'s `repeat`.
    #[must_use]
    pub fn unit(&self, task: &str, repeat: u32) -> PathBuf {
        self.dir.join("tasks").join(task).join(repeat.to_string())
    }

    /// Every `(task, repeat)` with a `result.json`, sorted.
    #[must_use]
    pub fn results(&self) -> Vec<TaskResult> {
        let mut out = Vec::new();
        for task in sorted_dirs(&self.dir.join("tasks")) {
            for repeat in sorted_dirs(&task) {
                if let Some(result) = read_json::<TaskResult>(&repeat.join("result.json")) {
                    out.push(result);
                }
            }
        }
        out.sort_by(|a, b| (&a.task_id, a.repeat).cmp(&(&b.task_id, b.repeat)));
        out
    }

    /// What a resumed run does with `task`'s `repeat`.
    #[must_use]
    pub fn resume(&self, task: &str, repeat: u32) -> Resume {
        let unit = self.unit(task, repeat);
        if !unit.exists() {
            return Resume::Run;
        }
        if let Some(skipped) =
            read_json::<TaskResult>(&unit.join("result.json")).and_then(|r| r.skipped)
        {
            return Resume::Rerun(format!("it was skipped ({skipped})"));
        }
        match read_json::<Done>(&unit.join("done.json")) {
            None => Resume::Rerun("no completion marker".to_owned()),
            Some(done) if done.fingerprint == fingerprint(&unit.join("deliverables")) => {
                Resume::Skip
            }
            Some(_) => Resume::Rerun("deliverables changed since completion".to_owned()),
        }
    }

    /// Writes `result` and, when it executed, the completion marker.
    ///
    /// # Errors
    ///
    /// `eval.io`.
    pub fn finish(&self, result: &TaskResult) -> Result<(), Error> {
        let unit = self.unit(&result.task_id, result.repeat);
        write_json(&unit.join("result.json"), result)?;
        if result.skipped.is_some() {
            return Ok(());
        }
        let done = Done {
            fingerprint: fingerprint(&unit.join("deliverables")),
            finished_at: jiff::Timestamp::now().to_string(),
        };
        write_json(&unit.join("done.json"), &done)
    }
}

fn sorted_dirs(dir: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    dirs
}

/// sha256 over every file below `dir` (relative path and bytes), so any
/// edit, addition or removal changes it.
#[must_use]
pub fn fingerprint(dir: &Path) -> String {
    let mut files = Vec::new();
    collect(dir, dir, &mut files);
    files.sort();
    let mut hash = Sha256::new();
    for (rel, path) in files {
        hash.update(rel.as_bytes());
        hash.update([0]);
        hash.update(std::fs::read(&path).unwrap_or_default());
        hash.update([0]);
    }
    data_hex(&hash.finalize())
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(root, &path, out);
        } else if let Ok(rel) = path.strip_prefix(root) {
            out.push((rel.to_string_lossy().replace('\\', "/"), path));
        }
    }
}

/// The lower-case hex sha256 of `bytes`.
#[must_use]
pub fn digest(bytes: &[u8]) -> String {
    data_hex(&Sha256::digest(bytes))
}

fn data_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// Reads JSON at `path` (`None` when absent or unreadable).
#[must_use]
pub fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// Writes `value` as pretty JSON at `path`, creating its directory.
///
/// # Errors
///
/// `eval.io`.
pub fn write_json(path: &Path, value: &impl Serialize) -> Result<(), Error> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::io(&codes::EVAL_IO, parent, &e))?;
    }
    let text = serde_json::to_string_pretty(value).unwrap_or_default();
    std::fs::write(path, text + "\n").map_err(|e| Error::io(&codes::EVAL_IO, path, &e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evolution_sees_only_the_five_fields() {
        let result = TaskResult {
            task_id: "t1".into(),
            contaminated: true,
            exit_status: Some(0),
            ..TaskResult::default()
        };
        let view = result.for_evolution();
        let keys: Vec<&String> = view.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["cost_usd", "delivered", "error", "ok", "wall_secs"]);
    }
}

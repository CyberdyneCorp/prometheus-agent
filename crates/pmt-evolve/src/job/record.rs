//! Job records: `<profile>/evolve/jobs/<job_id>/job.toml` beside the
//! frozen `task/`. A job directory appears whole: it is built under a
//! temporary name and renamed into place.

use std::path::{Path, PathBuf};

use pmt_core::error::{Error, codes};
use serde::{Deserialize, Serialize};

/// What queued a job. Every trigger produces the same record and goes
/// through the same budget and adoption rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    /// `prometheus evolve run`.
    Cli,
    /// `/evolve` in a chat.
    Slash,
    /// A scheduled job with `kind = "evolve"`.
    Cron,
    /// A kanban task of kind `evolve`.
    Kanban,
    /// After a failed or long session (`evolve.auto`).
    Auto,
}

impl Trigger {
    /// The spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cli => "cli",
            Self::Slash => "slash",
            Self::Cron => "cron",
            Self::Kanban => "kanban",
            Self::Auto => "auto",
        }
    }
}

/// One mode's spend, fixed by k.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanSummary {
    /// `parallel` or `sequential`.
    pub mode: String,
    /// R = 4k.
    pub rollouts: usize,
    /// P = 3k.
    pub sessions: usize,
}

/// `job.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobRecord {
    /// `evo_<ULID>`.
    pub id: String,
    /// `queued` when written.
    pub status: String,
    /// What queued it.
    pub trigger: Trigger,
    /// The scheduled job that fired it (`trigger = "cron"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cron_job_id: Option<String>,
    /// The kanban task it serves (`trigger = "kanban"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kanban_task_id: Option<String>,
    /// The session the task came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_session: Option<String>,
    /// `parallel`, `sequential` or `both`.
    pub mode: String,
    /// The recursion budget.
    pub k: u8,
    /// The experience-library domain.
    pub domain: String,
    /// `--budget-usd`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_usd: Option<f64>,
    /// RFC 3339.
    pub created_at: String,
    /// What each mode spends.
    pub plan: Vec<PlanSummary>,
}

/// `<profile>/evolve/jobs`.
#[derive(Debug, Clone)]
pub struct Jobs {
    root: PathBuf,
}

const JOB_FILE: &str = "job.toml";

impl Jobs {
    /// The jobs of the profile at `profile_dir`.
    #[must_use]
    pub fn of(profile_dir: &Path) -> Self {
        Self {
            root: profile_dir.join("evolve").join("jobs"),
        }
    }

    /// The directory of job `id`.
    #[must_use]
    pub fn dir(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }

    /// A fresh job id.
    #[must_use]
    pub fn new_id() -> String {
        format!("evo_{}", ulid::Ulid::generate())
    }

    /// Creates the directory of `record` with `fill` writing its `task/`,
    /// then publishes it whole. Nothing is left behind on failure.
    ///
    /// # Errors
    ///
    /// What `fill` fails with; `harness.io`.
    pub fn create(
        &self,
        record: &JobRecord,
        fill: impl FnOnce(&Path) -> Result<(), Error>,
    ) -> Result<PathBuf, Error> {
        std::fs::create_dir_all(&self.root).map_err(|e| io(&self.root, &e))?;
        let staging = self.root.join(format!(".tmp-{}", record.id));
        std::fs::create_dir_all(staging.join("task")).map_err(|e| io(&staging, &e))?;
        let built = fill(&staging.join("task")).and_then(|()| {
            let text = toml::to_string(record)
                .map_err(|e| Error::new(&codes::HARNESS_IO, e.to_string()))?;
            std::fs::write(staging.join(JOB_FILE), text).map_err(|e| io(&staging, &e))
        });
        let target = self.dir(&record.id);
        let published =
            built.and_then(|()| std::fs::rename(&staging, &target).map_err(|e| io(&target, &e)));
        if published.is_err() {
            let _ = std::fs::remove_dir_all(&staging);
        }
        published.map(|()| target)
    }

    /// Job `id`.
    ///
    /// # Errors
    ///
    /// `evolve.job_not_found`; `harness.io`.
    pub fn get(&self, id: &str) -> Result<JobRecord, Error> {
        let path = self.dir(id).join(JOB_FILE);
        let text = std::fs::read_to_string(&path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => Error::new(
                &codes::EVOLVE_JOB_NOT_FOUND,
                format!("no evolution job '{id}'"),
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

    /// Rewrites the status of job `id` in its `job.toml`.
    ///
    /// # Errors
    ///
    /// `evolve.job_not_found`; `harness.io`.
    pub fn set_status(&self, id: &str, status: &str) -> Result<(), Error> {
        self.edit(id, |r| status.clone_into(&mut r.status))
    }

    /// Records the domain classification resolved for job `id`.
    ///
    /// # Errors
    ///
    /// `evolve.job_not_found`; `harness.io`.
    pub fn set_domain(&self, id: &str, domain: &str) -> Result<(), Error> {
        self.edit(id, |r| domain.clone_into(&mut r.domain))
    }

    fn edit(&self, id: &str, change: impl FnOnce(&mut JobRecord)) -> Result<(), Error> {
        let mut record = self.get(id)?;
        change(&mut record);
        let path = self.dir(id).join(JOB_FILE);
        let text =
            toml::to_string(&record).map_err(|e| Error::new(&codes::HARNESS_IO, e.to_string()))?;
        pmt_core::private_fs::write_atomic(&path, text.as_bytes()).map_err(|e| io(&path, &e))
    }

    /// Records the user's pick `id` for the first mode of job `job` that
    /// awaits a selection, and queues the job again.
    ///
    /// # Errors
    ///
    /// `evolve.job_not_found`; `evolve.job_state` when the job does not
    /// await a selection or `id` names no candidate.
    pub fn choose(&self, job: &str, id: &str) -> Result<String, Error> {
        let record = self.get(job)?;
        let summary = crate::engine::summary::JobSummary::read(&self.dir(job));
        let mode = summary
            .as_ref()
            .filter(|_| record.status == "awaiting-selection")
            .and_then(|s| s.modes.iter().find(|m| m.status == "awaiting-selection"))
            .ok_or_else(|| {
                state(format!(
                    "job {job} is {}, not awaiting-selection",
                    record.status
                ))
            })?;
        if !mode.candidates.iter().any(|(_, c)| c == id) {
            let ids: Vec<&str> = mode.candidates.iter().map(|(_, c)| c.as_str()).collect();
            return Err(state(format!(
                "no candidate '{id}' (candidates: {})",
                ids.join(", ")
            )));
        }
        let path = self
            .dir(job)
            .join("modes")
            .join(&mode.mode)
            .join("choice.toml");
        let text = toml::to_string(&crate::engine::select::UserChoice { id: id.to_owned() })
            .map_err(|e| Error::new(&codes::HARNESS_IO, e.to_string()))?;
        pmt_core::private_fs::write_atomic(&path, text.as_bytes()).map_err(|e| io(&path, &e))?;
        self.set_status(job, "queued")?;
        Ok(mode.mode.clone())
    }

    /// Queues an unfinished (or finished) job again so it resumes from
    /// its caches.
    ///
    /// # Errors
    ///
    /// `evolve.job_not_found`; `evolve.job_state` while it runs.
    pub fn resume(&self, job: &str) -> Result<(), Error> {
        self.get(job)?;
        if crate::engine::is_running(&self.dir(job)) {
            return Err(state(format!("job {job} is running")));
        }
        self.set_status(job, "queued")
    }

    /// Cancels job `job`: a running job stops at its next model-call
    /// boundary; a queued or paused one ends `cancelled` at once. Returns
    /// whether it was running.
    ///
    /// # Errors
    ///
    /// `evolve.job_not_found`; `evolve.job_state` when it already ended.
    pub fn cancel(&self, job: &str) -> Result<bool, Error> {
        let record = self.get(job)?;
        let dir = self.dir(job);
        if crate::engine::is_running(&dir) {
            let path = dir.join(crate::engine::CANCEL_FILE);
            std::fs::write(&path, b"").map_err(|e| io(&path, &e))?;
            return Ok(true);
        }
        if !matches!(
            record.status.as_str(),
            "queued" | "running" | "awaiting-selection"
        ) {
            return Err(state(format!(
                "job {job} already ended ({})",
                record.status
            )));
        }
        self.set_status(job, "cancelled")?;
        if let Some(mut summary) = crate::engine::summary::JobSummary::read(&dir) {
            "cancelled".clone_into(&mut summary.status);
            summary.write(&dir)?;
        }
        Ok(false)
    }

    /// Jobs left `running` by a process that died become `queued` again
    /// (the daemon calls this at start when `evolve.resume_on_start`).
    ///
    /// # Errors
    ///
    /// `harness.io`.
    pub fn requeue_interrupted(&self) -> Result<Vec<String>, Error> {
        let mut requeued = Vec::new();
        for job in self.list()? {
            if job.status == "running" && !crate::engine::is_running(&self.dir(&job.id)) {
                self.set_status(&job.id, "queued")?;
                requeued.push(job.id);
            }
        }
        Ok(requeued)
    }

    /// Every job, oldest first (ids are ULIDs).
    ///
    /// # Errors
    ///
    /// `harness.io`.
    pub fn list(&self) -> Result<Vec<JobRecord>, Error> {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return Ok(Vec::new());
        };
        let mut ids: Vec<String> = entries
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("evo_"))
            .collect();
        ids.sort();
        ids.iter().map(|id| self.get(id)).collect()
    }
}

fn state(message: String) -> Error {
    Error::new(&codes::EVOLVE_JOB_STATE, message)
}

fn io(path: &Path, err: &std::io::Error) -> Error {
    Error::io(&codes::HARNESS_IO, path, err)
}

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

fn io(path: &Path, err: &std::io::Error) -> Error {
    Error::io(&codes::HARNESS_IO, path, err)
}

//! Download jobs on disk under `<home>/cache/partials/`.

use std::path::{Path, PathBuf};

use pmt_core::error::{Error, codes};
use serde::{Deserialize, Serialize};

use super::io_error;

/// One file a job downloads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileSpec {
    /// Display name (the file name).
    pub name: String,
    /// Where it is downloaded from.
    pub url: String,
    /// Size in bytes.
    pub size: u64,
    /// Lower-case hex SHA-256.
    pub sha256: String,
    /// Where the verified file is published.
    pub target: PathBuf,
}

/// One file of a job and whether it is in place.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobFile {
    /// The file.
    pub spec: FileSpec,
    /// Verified and published.
    pub done: bool,
}

/// Where a job is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    /// A process is downloading it (or was, see [`JobView::state`]).
    Running,
    /// Paused at a chunk boundary.
    Paused,
    /// Stopped by an error; `resume` retries it.
    Failed,
    /// Every file is in place.
    Done,
}

/// The process running a job (pid plus its OS start time).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Owner {
    /// Process id.
    pub pid: u32,
    /// OS start time of that pid.
    pub start_time: u64,
}

/// A download job.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Job {
    /// Short id (`prometheus local resume <id>`).
    pub id: String,
    /// What it downloads: a model id, or `engine`.
    pub subject: String,
    /// Its files, in download order.
    pub files: Vec<JobFile>,
    /// Where it is.
    pub state: JobState,
    /// RFC 3339 creation time.
    pub created_at: String,
    /// The process running it, while it runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<Owner>,
    /// The failure, when it failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// What the job publishes once done (the library record of a model).
    #[serde(default)]
    pub meta: serde_json::Value,
}

impl Job {
    /// Bytes of every file.
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(|f| f.spec.size).sum()
    }

    /// Bytes of the finished files before file `index`.
    #[must_use]
    pub fn done_bytes_before(&self, index: usize) -> u64 {
        self.files[..index]
            .iter()
            .filter(|f| f.done)
            .map(|f| f.spec.size)
            .sum()
    }

    pub(super) fn start(&mut self) {
        self.state = JobState::Running;
        self.error = None;
        let pid = std::process::id();
        self.owner =
            pmt_core::runtime::process_start_time(pid).map(|start_time| Owner { pid, start_time });
    }

    pub(super) fn fail(&mut self, reason: &str) {
        self.state = JobState::Failed;
        self.error = Some(reason.to_owned());
    }
}

/// A job as `prometheus local jobs` lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct JobView {
    /// Job id.
    pub id: String,
    /// Model id or `engine`.
    pub subject: String,
    /// `running`, `paused`, `interrupted` (its process died), `failed` or
    /// `done`.
    pub state: &'static str,
    /// Bytes on disk.
    pub done_bytes: u64,
    /// Bytes of the whole job.
    pub total_bytes: u64,
    /// The failure, when it failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The jobs of one home.
#[derive(Debug, Clone)]
pub struct Jobs {
    dir: PathBuf,
}

impl Jobs {
    /// `<home>/cache/partials`.
    #[must_use]
    pub fn open(home: &Path) -> Self {
        Self {
            dir: home.join("cache").join("partials"),
        }
    }

    /// The directory.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// A new paused-until-run job for `subject`, saved.
    ///
    /// # Errors
    ///
    /// `runtime.io` when it cannot be written.
    ///
    /// # Panics
    ///
    /// Never: the record serializes.
    pub fn create(
        &self,
        subject: &str,
        files: Vec<FileSpec>,
        meta: serde_json::Value,
    ) -> Result<Job, Error> {
        let ulid = ulid::Ulid::generate().to_string().to_lowercase();
        let job = Job {
            id: ulid[ulid.len() - 8..].to_owned(),
            subject: subject.to_owned(),
            files: files
                .into_iter()
                .map(|spec| JobFile { spec, done: false })
                .collect(),
            state: JobState::Paused,
            created_at: jiff::Timestamp::now().to_string(),
            owner: None,
            error: None,
            meta,
        };
        self.save(&job)?;
        Ok(job)
    }

    /// Writes `job` (atomically).
    ///
    /// # Errors
    ///
    /// `runtime.io`.
    ///
    /// # Panics
    ///
    /// Never: the record serializes.
    pub fn save(&self, job: &Job) -> Result<(), Error> {
        std::fs::create_dir_all(&self.dir).map_err(|e| io_error(&self.dir, &e))?;
        let path = self.path(&job.id);
        let tmp = path.with_extension("json.tmp");
        let text = serde_json::to_vec_pretty(job).expect("a job serializes");
        std::fs::write(&tmp, text).map_err(|e| io_error(&tmp, &e))?;
        std::fs::rename(&tmp, &path).map_err(|e| io_error(&path, &e))
    }

    /// The job `id`.
    ///
    /// # Errors
    ///
    /// `local.not_found` (66).
    pub fn load(&self, id: &str) -> Result<Job, Error> {
        std::fs::read_to_string(self.path(id))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .ok_or_else(|| {
                Error::new(&codes::LOCAL_NOT_FOUND, format!("no download job '{id}'"))
                    .with_hint("prometheus local jobs lists them")
            })
    }

    /// Every job, oldest first.
    #[must_use]
    pub fn all(&self) -> Vec<Job> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut jobs: Vec<Job> = entries
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .filter_map(|e| std::fs::read_to_string(e.path()).ok())
            .filter_map(|text| serde_json::from_str(&text).ok())
            .collect();
        jobs.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        jobs
    }

    /// The unfinished job of `subject`, if any.
    #[must_use]
    pub fn unfinished(&self, subject: &str) -> Option<Job> {
        self.all()
            .into_iter()
            .find(|j| j.subject == subject && j.state != JobState::Done)
    }

    /// Every job as listed.
    #[must_use]
    pub fn views(&self) -> Vec<JobView> {
        self.all().iter().map(|job| self.view(job)).collect()
    }

    /// How `job` is listed: a running job whose process is gone is
    /// `interrupted`.
    #[must_use]
    pub fn view(&self, job: &Job) -> JobView {
        let done_bytes = job
            .files
            .iter()
            .enumerate()
            .map(|(i, f)| {
                if f.done {
                    f.spec.size
                } else {
                    std::fs::metadata(self.partial(&job.id, i)).map_or(0, |m| m.len())
                }
            })
            .sum();
        let state = match job.state {
            JobState::Running if job.owner.is_some_and(alive) => "running",
            JobState::Running => "interrupted",
            JobState::Paused => "paused",
            JobState::Failed => "failed",
            JobState::Done => "done",
        };
        JobView {
            id: job.id.clone(),
            subject: job.subject.clone(),
            state,
            done_bytes,
            total_bytes: job.total_bytes(),
            error: job.error.clone(),
        }
    }

    /// Deletes a job and its partial files.
    ///
    /// # Errors
    ///
    /// `runtime.io`.
    pub fn remove(&self, job: &Job) -> Result<(), Error> {
        for index in 0..job.files.len() {
            let _ = std::fs::remove_file(self.partial(&job.id, index));
        }
        let path = self.path(&job.id);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io_error(&path, &e)),
        }
    }

    /// Pauses job `id`: a job another process runs is asked through its
    /// marker file and stops at its next chunk; any other unfinished job
    /// is marked paused at once.
    ///
    /// # Errors
    ///
    /// `local.not_found` (66); `runtime.io`.
    pub fn pause(&self, id: &str) -> Result<JobView, Error> {
        let mut job = self.load(id)?;
        let view = self.view(&job);
        match view.state {
            "running" => {
                let marker = self.pause_marker(id);
                std::fs::write(&marker, b"").map_err(|e| io_error(&marker, &e))?;
                Ok(JobView {
                    state: "pausing",
                    ..view
                })
            }
            "done" => Ok(view),
            _ => {
                job.state = JobState::Paused;
                job.owner = None;
                self.save(&job)?;
                Ok(self.view(&job))
            }
        }
    }

    /// The marker asking the process running job `id` to pause it.
    #[must_use]
    pub fn pause_marker(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.pause"))
    }

    /// The partial file of file `index` of job `id`.
    #[must_use]
    pub fn partial(&self, id: &str, index: usize) -> PathBuf {
        self.dir.join(format!("{id}-{index}.part"))
    }

    /// Takes the job's lock: one process runs a job at a time.
    pub(super) fn lock(&self, id: &str) -> Result<std::fs::File, Error> {
        std::fs::create_dir_all(&self.dir).map_err(|e| io_error(&self.dir, &e))?;
        let path = self.dir.join(format!("{id}.lock"));
        let file = std::fs::File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|e| io_error(&path, &e))?;
        match pmt_core::file_lock::try_lock(&file) {
            Ok(()) => Ok(file),
            Err(std::fs::TryLockError::WouldBlock) => Err(Error::new(
                &codes::LOCAL_BUSY,
                format!("download job '{id}' is running in another process"),
            )),
            Err(std::fs::TryLockError::Error(e)) => Err(io_error(&path, &e)),
        }
    }

    fn path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }
}

fn alive(owner: Owner) -> bool {
    pmt_core::runtime::process_start_time(owner.pid) == Some(owner.start_time)
}

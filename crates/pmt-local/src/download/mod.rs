//! The one downloader of local models (spec: local-models, Resumable,
//! verifiable downloads): engine archives, model weights, split GGUF
//! parts, vision projectors and draft models all go through a [`Job`].
//!
//! ```text
//! <home>/cache/partials/<job>.json     the job: its files, state and owner
//! <home>/cache/partials/<job>-<n>.part the bytes of file n so far
//! ```
//!
//! A file is fetched into its partial (resuming the missing range from a
//! host that honours HTTP `Range`; a host that answers 200 restarts that
//! file only), then its size and SHA-256 are checked, and only then is it
//! renamed to its target. A target is therefore never an incomplete file.
//! A job is paused at a chunk boundary through its [`Control`], or from
//! another process through a `<job>.pause` marker the running process
//! checks a few times a second; the job file keeps it paused across
//! restarts until it is resumed.

mod jobs;

use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use pmt_core::error::{Error, codes};
use pmt_update::download::{Budget, FileSink, Sink};

pub use jobs::{FileSpec, Job, JobFile, JobState, JobView, Jobs, Owner};

/// Pauses a running job at its next chunk boundary.
#[derive(Debug, Clone, Default)]
pub struct Control(Arc<AtomicBool>);

impl Control {
    /// Asks the job to stop at the next chunk boundary.
    pub fn pause(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Whether a pause was asked for.
    #[must_use]
    pub fn paused(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// How often a running job looks for a pause asked by another process.
const MARKER_POLL: std::time::Duration = std::time::Duration::from_millis(250);

/// Byte progress of a whole job: finished and resumed bytes included.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    /// Bytes on disk.
    pub done: u64,
    /// Bytes of every file of the job.
    pub total: u64,
}

impl Progress {
    /// Whole percent done.
    #[must_use]
    pub fn percent(&self) -> u64 {
        (self.done * 100).checked_div(self.total).unwrap_or(100)
    }
}

/// How a run ended without an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finished {
    /// Every file is verified and in place.
    Done,
    /// Paused at a chunk boundary; `resume` continues it.
    Paused,
}

/// Downloads jobs with one HTTP client.
#[derive(Debug, Clone)]
pub struct Downloader {
    client: reqwest::Client,
    budget: Budget,
}

impl Downloader {
    /// A downloader using `client`.
    #[must_use]
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            client,
            budget: Budget::default(),
        }
    }

    /// Replaces the retry budget (tests use short delays).
    #[must_use]
    pub fn with_budget(mut self, budget: Budget) -> Self {
        self.budget = budget;
        self
    }

    /// Runs `job` (stored in `jobs`) until it is done, paused or fails;
    /// the job file is updated at each step.
    ///
    /// # Errors
    ///
    /// `runtime.download` (69) after the retry budget,
    /// `local.digest_mismatch` (74) naming a file that differs from its
    /// pin (the file is deleted), `local.io` errors of the partials,
    /// `cli.conflict` when another process runs the job.
    pub async fn run(
        &self,
        jobs: &Jobs,
        job: &mut Job,
        control: &Control,
        progress: &mut (dyn FnMut(Progress) + Send),
    ) -> Result<Finished, Error> {
        let _lock = jobs.lock(&job.id)?;
        let _ = std::fs::remove_file(jobs.pause_marker(&job.id));
        job.start();
        jobs.save(job)?;
        let result = self.files(jobs, job, control, progress).await;
        match &result {
            Ok(Finished::Done) => job.state = JobState::Done,
            Ok(Finished::Paused) => {
                job.state = JobState::Paused;
                let _ = std::fs::remove_file(jobs.pause_marker(&job.id));
            }
            Err(err) => job.fail(err.message()),
        }
        job.owner = None;
        jobs.save(job)?;
        result
    }

    async fn files(
        &self,
        jobs: &Jobs,
        job: &mut Job,
        control: &Control,
        progress: &mut (dyn FnMut(Progress) + Send),
    ) -> Result<Finished, Error> {
        let total = job.total_bytes();
        for index in 0..job.files.len() {
            if job.files[index].done || reuse(&job.files[index].spec) {
                job.files[index].done = true;
                jobs.save(job)?;
                continue;
            }
            let before = job.done_bytes_before(index);
            let partial = jobs.partial(&job.id, index);
            let spec = job.files[index].spec.clone();
            let mut sink = PauseSink {
                inner: FileSink::open(&partial).map_err(|e| io_error(&partial, &e))?,
                control,
                marker: jobs.pause_marker(&job.id),
                checked: std::time::Instant::now(),
                report: &mut |written| {
                    progress(Progress {
                        done: before + written,
                        total,
                    });
                },
            };
            (sink.report)(sink.inner.written());
            if sink.inner.written() != spec.size {
                let fetched =
                    pmt_update::download::fetch(&self.client, &spec.url, &mut sink, &self.budget)
                        .await;
                if control.paused() {
                    return Ok(Finished::Paused);
                }
                fetched.map_err(|reason| {
                    Error::new(
                        &codes::RUNTIME_DOWNLOAD,
                        format!("cannot download {}: {reason}", spec.url),
                    )
                })?;
            }
            sink.inner.sync().map_err(|e| io_error(&partial, &e))?;
            drop(sink);
            verify(&partial, &spec)?;
            publish(&partial, &spec.target)?;
            job.files[index].done = true;
            jobs.save(job)?;
        }
        progress(Progress { done: total, total });
        Ok(Finished::Done)
    }
}

/// A target an earlier job already published (same size) is reused.
fn reuse(spec: &FileSpec) -> bool {
    std::fs::metadata(&spec.target).is_ok_and(|m| m.len() == spec.size)
}

/// Writes through to the partial file, reports progress, and refuses the
/// next chunk once a pause is asked for (so the file ends on a chunk).
struct PauseSink<'a> {
    inner: FileSink,
    control: &'a Control,
    marker: std::path::PathBuf,
    checked: std::time::Instant,
    report: &'a mut (dyn FnMut(u64) + Send),
}

impl Sink for PauseSink<'_> {
    fn written(&self) -> u64 {
        self.inner.written()
    }

    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.checked.elapsed() >= MARKER_POLL {
            self.checked = std::time::Instant::now();
            if self.marker.exists() {
                self.control.pause();
            }
        }
        if self.control.paused() {
            return Err(io::Error::other("paused"));
        }
        self.inner.append(bytes)?;
        (self.report)(self.inner.written());
        Ok(())
    }

    fn reset(&mut self) -> io::Result<()> {
        self.inner.reset()?;
        (self.report)(0);
        Ok(())
    }
}

/// Fails, deleting the partial, unless it has the pinned size and digest.
fn verify(partial: &Path, spec: &FileSpec) -> Result<(), Error> {
    let size = std::fs::metadata(partial)
        .map_err(|e| io_error(partial, &e))?
        .len();
    let actual = if size == spec.size {
        pmt_update::runtimes::hash_file(partial).map_err(|e| io_error(partial, &e))?
    } else {
        format!("{size} bytes")
    };
    if actual == spec.sha256 {
        return Ok(());
    }
    let _ = std::fs::remove_file(partial);
    let expected = if size == spec.size {
        spec.sha256.clone()
    } else {
        format!("{} bytes", spec.size)
    };
    Err(Error::new(
        &codes::LOCAL_DIGEST_MISMATCH,
        format!(
            "sha256 mismatch for {}: pinned {expected}, got {actual}",
            spec.name
        ),
    )
    .with_hint("the file was deleted and nothing was installed; run the command again"))
}

/// Moves a verified partial to its target.
fn publish(partial: &Path, target: &Path) -> Result<(), Error> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| io_error(parent, &e))?;
    }
    std::fs::rename(partial, target).map_err(|e| io_error(target, &e))
}

pub(crate) fn io_error(path: &Path, e: &io::Error) -> Error {
    Error::io(&codes::RUNTIME_IO, path, e)
}

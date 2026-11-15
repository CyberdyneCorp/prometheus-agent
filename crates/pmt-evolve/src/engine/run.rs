//! One run of a job: its gate, caches, log and summary, and the two
//! primitive steps everything else is built from — an improver session
//! ([`Run::session`]: cache, retry once on transport failure or an empty
//! reply, the session bound) and a rollout ([`Run::rollout`]: cache, the
//! rollout bound, recorded in the job directory).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use pmt_config::harness::Tree;

use super::cache::{Cache, SessionKey, SessionMeta};
use super::events::JobLog;
use super::gate::{JobGate, Stop};
use super::ledger::Kind;
use super::ports::{ImproverRequest, RolloutOutcome, RolloutRequest, SessionKind};
use super::summary::JobSummary;
use super::tripwire::Tripwire;
use super::{Engine, evidence};
use crate::job::JobRecord;

/// A session that ended with a reply (possibly empty).
#[derive(Debug, Clone)]
pub(crate) struct SessionDone {
    pub text: String,
    pub usd: f64,
    pub cached: bool,
}

/// A session that did not.
#[derive(Debug, Clone)]
pub(crate) enum SessionError {
    /// The job stopped before or during it.
    Stopped(Stop),
    /// It failed (after its retry for a transport failure).
    Failed { message: String, usd: f64 },
}

/// One improver session to run.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SessionCall<'a> {
    pub label: &'a str,
    pub kind: SessionKind,
    pub system: &'a str,
    pub mission: &'a str,
    pub workspace: &'a Path,
}

/// A finished rollout.
#[derive(Debug, Clone)]
pub(crate) struct RolloutDone {
    pub label: String,
    pub outcome: RolloutOutcome,
    pub cached: bool,
    /// Its images (in the cache, or the job directory).
    pub images: PathBuf,
}

/// The state of one job run.
pub(crate) struct Run<'e> {
    pub engine: &'e Engine<'e>,
    pub job: JobRecord,
    pub dir: PathBuf,
    pub task_fingerprint: String,
    pub gate: Arc<JobGate>,
    pub log: JobLog,
    pub tripwire: Tripwire,
    pub cache: Cache,
    /// The harness the job started from.
    pub active: Tree,
    pub summary: Mutex<JobSummary>,
    pub model_calls: AtomicU32,
    pub rollouts_executed: AtomicU32,
}

impl Run<'_> {
    /// Applies `edit` to the summary and writes it.
    pub(crate) fn update(&self, edit: impl FnOnce(&mut JobSummary)) {
        let mut summary = self
            .summary
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        edit(&mut summary);
        summary.costs = self.gate.costs();
        summary.model_calls = self.model_calls.load(Ordering::Relaxed);
        summary.rollouts_run = self.rollouts_executed.load(Ordering::Relaxed);
        if let Err(err) = summary.write(&self.dir) {
            tracing::warn!(job = %self.job.id, error = %err, "summary.json not written");
        }
    }

    /// A clean scratch directory under `<job>/tmp`.
    pub(crate) fn scratch(&self, name: &str) -> std::io::Result<PathBuf> {
        let dir = self.dir.join("tmp").join(name);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
        pmt_core::private_fs::create_dir(&dir)?;
        Ok(dir)
    }

    /// Runs one improver session in `call.workspace` (which holds
    /// `context/`); its edits stay there.
    pub(crate) async fn session(&self, call: SessionCall<'_>) -> Result<SessionDone, SessionError> {
        let model = self.engine.improver.model_id();
        let key = Cache::session_key(&SessionKey {
            label: call.label,
            mission: call.mission,
            system: call.system,
            model: &model,
            evidence: call.workspace,
        });
        if let Some(hit) = self.cache.session(&key) {
            restore_harness(call.workspace, &hit.workspace).map_err(|e| SessionError::Failed {
                message: format!("cached session not restored: {e}"),
                usd: 0.0,
            })?;
            return Ok(SessionDone {
                text: hit.response,
                usd: 0.0,
                cached: true,
            });
        }
        self.gate.admit().map_err(SessionError::Stopped)?;
        let _permit = self
            .engine
            .limits
            .sessions
            .acquire()
            .await
            .map_err(|_| SessionError::Stopped(Stop::Cancelled))?;
        let done = self.call_improver(call).await?;
        if !done.text.trim().is_empty() {
            self.store_session(&key, call, &done, &model);
        }
        Ok(done)
    }

    /// Calls the improver, once more after `retry_delay` on a transport
    /// failure or an empty reply.
    async fn call_improver(&self, call: SessionCall<'_>) -> Result<SessionDone, SessionError> {
        let charge = match call.kind {
            SessionKind::Select => Kind::Selection,
            SessionKind::Reflect => Kind::Reflection,
            SessionKind::Propose | SessionKind::Compose => Kind::Session,
        };
        let mut usd = 0.0;
        let mut last = None;
        for attempt in 0..2 {
            if attempt > 0 {
                self.pause().await?;
            }
            self.gate.admit().map_err(SessionError::Stopped)?;
            self.model_calls.fetch_add(1, Ordering::Relaxed);
            let reply = self.engine.improver.run(self.request(call)).await;
            match reply {
                Ok(reply) => {
                    self.gate.charge(charge, reply.usd);
                    usd += reply.usd;
                    if !reply.text.trim().is_empty() {
                        return Ok(SessionDone {
                            text: reply.text,
                            usd,
                            cached: false,
                        });
                    }
                    last = None;
                }
                Err(failure) => {
                    self.gate.charge(charge, failure.usd);
                    usd += failure.usd;
                    if !failure.transport {
                        return Err(self.stopped_or(failure.message, usd));
                    }
                    last = Some(failure.message);
                }
            }
        }
        match last {
            Some(message) => Err(self.stopped_or(message, usd)),
            None => Ok(SessionDone {
                text: String::new(),
                usd,
                cached: false,
            }),
        }
    }

    fn stopped_or(&self, message: String, usd: f64) -> SessionError {
        match self.gate.stopped() {
            Some(stop) => SessionError::Stopped(stop),
            None => SessionError::Failed { message, usd },
        }
    }

    async fn pause(&self) -> Result<(), SessionError> {
        tokio::select! {
            () = tokio::time::sleep(self.engine.retry_delay) => Ok(()),
            () = self.gate.token().cancelled() => {
                Err(SessionError::Stopped(self.gate.stopped().unwrap_or(Stop::Cancelled)))
            }
        }
    }

    fn request<'a>(&'a self, call: SessionCall<'a>) -> ImproverRequest<'a> {
        let improver = &self.engine.config.improver;
        ImproverRequest {
            job_id: &self.job.id,
            label: call.label,
            kind: call.kind,
            system: call.system,
            mission: call.mission,
            workspace: call.workspace,
            max_iterations: improver.max_iterations,
            timeout: std::time::Duration::from_secs(improver.timeout_secs),
            cancel: self.gate.token(),
        }
    }

    fn store_session(&self, key: &str, call: SessionCall<'_>, done: &SessionDone, model: &str) {
        let harness = evidence::context(call.workspace).join("harness");
        let tree = Tree::read(&harness).map(|(t, _)| t).unwrap_or_default();
        let meta = SessionMeta {
            label: call.label.to_owned(),
            model: model.to_owned(),
            prompt_version: super::prompts::PROMPT_VERSION.to_owned(),
            usd: done.usd,
            workspace: String::new(),
        };
        if let Err(err) = self
            .cache
            .put_session(key, call.mission, &done.text, &meta, &tree)
        {
            tracing::warn!(job = %self.job.id, label = call.label, error = %err, "improver session not cached");
        }
    }

    /// Rolls `tree` out as `label`, recording it under `store`.
    pub(crate) async fn rollout(&self, label: &str, tree: &Tree, store: &Path) -> RolloutDone {
        let model = self.engine.rollouts.model_id();
        let key = Cache::rollout_key(
            &self.task_fingerprint,
            &tree.fingerprint(),
            &model,
            label,
            0,
        );
        let done = match self.cache.rollout(&key) {
            Some(outcome) => RolloutDone {
                label: label.to_owned(),
                outcome,
                cached: true,
                images: self.cache.rollout_dir(&key).join("images"),
            },
            None => self.fresh_rollout(label, tree, store, &key).await,
        };
        let recorded = serde_json::to_vec_pretty(&done.outcome).unwrap_or_default();
        let written = pmt_core::private_fs::create_dir(store)
            .and_then(|()| std::fs::write(store.join("rollout.json"), recorded));
        if let Err(err) = written {
            tracing::warn!(job = %self.job.id, label, error = %err, "rollout not recorded");
        }
        self.log.rollout(label, done.outcome.ok);
        done
    }

    async fn fresh_rollout(
        &self,
        label: &str,
        tree: &Tree,
        store: &Path,
        key: &str,
    ) -> RolloutDone {
        let images = store.join("images");
        let failed = |why: &str| RolloutDone {
            label: label.to_owned(),
            outcome: RolloutOutcome::failed(why),
            cached: false,
            images: images.clone(),
        };
        if let Err(stop) = self.gate.admit() {
            return failed(stop.status());
        }
        let Ok(_permit) = self.engine.limits.rollouts.acquire().await else {
            return failed("cancelled");
        };
        if let Err(stop) = self.gate.admit() {
            return failed(stop.status());
        }
        let harness = store.join("harness");
        let _ = std::fs::remove_dir_all(&harness);
        if let Err(err) = tree.write(&harness) {
            return failed(&format!("candidate harness not written: {err}"));
        }
        let _ = pmt_core::private_fs::create_dir(&images);
        self.rollouts_executed.fetch_add(1, Ordering::Relaxed);
        let outcome = self
            .engine
            .rollouts
            .run(RolloutRequest {
                job_id: &self.job.id,
                label,
                task: &self.dir.join("task"),
                harness: &harness,
                seed: 0,
                images: &images,
                cancel: self.gate.token(),
            })
            .await;
        self.gate.charge(Kind::Rollout, outcome.usd);
        // A rollout cut short by a stop is not a result worth keeping.
        if !self.gate.token().is_cancelled()
            && let Err(err) = self.cache.put_rollout(key, &outcome, &images)
        {
            tracing::warn!(job = %self.job.id, label, error = %err, "rollout not cached");
        }
        RolloutDone {
            label: label.to_owned(),
            outcome,
            cached: false,
            images,
        }
    }
}

/// Replaces `<workspace>/context/harness` with the cached `saved` tree.
fn restore_harness(workspace: &Path, saved: &Path) -> std::io::Result<()> {
    let harness = evidence::context(workspace).join("harness");
    if harness.exists() {
        std::fs::remove_dir_all(&harness)?;
    }
    pmt_core::private_fs::copy_tree(saved, &harness)
}

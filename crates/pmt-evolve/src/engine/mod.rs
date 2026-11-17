//! The evolution engine (spec: harness-evolution, Parallel mode;
//! Sequential mode; Isolated rollouts; Improver sessions; Stage
//! acceptance, retry and fallback; Evidence context; Budget ledger and
//! spend cap; Concurrency limits; Blind anonymised selection; Proposals
//! and adoption policy; Job lifecycle; Resume and content-addressed
//! session cache).
//!
//! [`Engine::run`] takes a queued job through its base rollouts, each
//! mode's stages and finals, selection and proposals. Rollouts and
//! improver sessions go through the host's [`ports`]; every result is
//! content-addressed ([`cache`]), so resuming a job replays what finished
//! for free and only runs what is missing.

pub mod cache;
pub mod events;
pub mod evidence;
pub mod gate;
pub mod ledger;
mod modes;
pub mod ports;
pub mod prompts;
mod reflect;
mod run;
pub mod select;
mod stage;
mod start;
pub mod summary;
pub mod tripwire;

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU32;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::future::join_all;
use pmt_config::harness::HarnessDir;
use pmt_config::schema::{EvolveConfig, EvolveMode, ExperienceConfig};
use pmt_core::error::{Error, codes};
use serde_json::json;

pub use modes::similarity;
pub use stage::{MAX_ERROR_CHARS, truncate};

use self::events::JobLog;
use self::gate::{JobGate, Limits};
use self::ledger::Ledger;
use self::modes::{Candidate, ModeRun};
use self::ports::{Improver, Rollouts};
use self::run::{RolloutDone, Run};
use self::select::Selection;
use self::summary::{BaseSummary, JobSummary};
use self::tripwire::Tripwire;
use crate::experience::lesson::Distilled;
use crate::harness::Rules;
use crate::job::budget::{self, ModePlan};
use crate::job::{JobRecord, Jobs};
use crate::proposal::{self, NewProposal, Proposals, Verdict};

/// The error of a job whose base rollouts all failed.
pub const BASE_FAILED: &str = "no base rollout produced anything";

/// The file whose presence cancels a running job.
pub const CANCEL_FILE: &str = "CANCEL";

/// Everything a job runs with.
pub struct Engine<'a> {
    /// The profile directory.
    pub profile_dir: &'a Path,
    /// `[evolve]`.
    pub config: &'a EvolveConfig,
    /// Harness validation rules.
    pub rules: &'a Rules,
    /// Runs rollouts.
    pub rollouts: &'a dyn Rollouts,
    /// Runs improver sessions.
    pub improver: &'a dyn Improver,
    /// The process-wide bounds.
    pub limits: Arc<Limits>,
    /// The wait before a session's retry (10 s).
    pub retry_delay: Duration,
    /// `[experience]`.
    pub experience: &'a ExperienceConfig,
    /// The spend estimate printed before the job starts.
    pub estimate_usd: f64,
}

/// What the adoption policy did with a mode's proposal.
#[derive(Debug, Clone)]
pub struct ProposalOutcome {
    /// The proposal.
    pub proposal: proposal::Proposal,
    /// It was adopted at once (`auto`): the version written.
    pub adopted: Option<proposal::Adopted>,
}

/// A mode's chosen final, with what its reflection distilled.
#[derive(Debug, Clone)]
pub(crate) struct Pick {
    pub mode: String,
    pub candidate: Candidate,
    pub selected: summary::Selected,
    pub distilled: Distilled,
}

/// How a job run ended.
#[derive(Debug, Clone)]
pub struct Finished {
    /// The summary as written.
    pub summary: JobSummary,
    /// The proposals it produced.
    pub proposals: Vec<ProposalOutcome>,
    /// What the user should be told.
    pub notices: Vec<String>,
}

impl Finished {
    /// The adoption the `auto` policy made, if any.
    #[must_use]
    pub fn adopted(&self) -> Option<&proposal::Adopted> {
        self.proposals.iter().find_map(|p| p.adopted.as_ref())
    }
}

impl Engine<'_> {
    /// Runs job `job_id` to its end (or to `awaiting-selection`).
    ///
    /// # Errors
    ///
    /// `evolve.job_not_found`, `evolve.job_running` when another process
    /// runs it, `harness.io`.
    pub async fn run(&self, job_id: &str) -> Result<Finished, Error> {
        let jobs = Jobs::of(self.profile_dir);
        let job = jobs.get(job_id)?;
        let dir = jobs.dir(job_id);
        let _lock = lock(&dir)?;
        let _ = std::fs::remove_file(dir.join(CANCEL_FILE));
        let mut run = self.open(job, dir)?;
        if run.job.domain.is_empty() {
            run.classify().await?;
        }
        // One job per domain at a time, in queue order: the library
        // snapshot of a job then holds exactly what was adopted before it
        // started (spec: experience-library, Stream order within a domain).
        let key = format!("{}|{}", self.profile_dir.display(), run.job.domain);
        let _turn = self.limits.domain(&key).await;
        let _domain = start::domain_lock(self.profile_dir, &run.job.domain).await?;
        run.library = run.prepare_library()?;
        let watcher = watch_cancel(run.dir.join(CANCEL_FILE), Arc::clone(&run.gate));
        let finished = run.execute().await;
        watcher.abort();
        let _ = std::fs::remove_dir_all(run.dir.join("tmp"));
        finished
    }

    fn open(&self, job: JobRecord, dir: PathBuf) -> Result<Run<'_>, Error> {
        let meta = std::fs::read_to_string(dir.join("task").join("task.toml"))
            .ok()
            .and_then(|t| toml::from_str::<crate::job::task::TaskMeta>(&t).ok())
            .unwrap_or_default();
        let previous = JobSummary::read(&dir);
        let harness = HarnessDir::of(self.profile_dir);
        let base_version = match previous.as_ref().and_then(|s| s.base_version.clone()) {
            Some(version) => version,
            None => harness.active_id()?,
        };
        let active = harness.tree(&base_version)?;
        let summary = JobSummary {
            job_id: job.id.clone(),
            status: "running".to_owned(),
            base_version: Some(base_version),
            estimate_usd: self.estimate_usd,
            ..JobSummary::default()
        };
        let cap = job.budget_usd.unwrap_or(self.config.budget.max_usd);
        let title = std::fs::read_to_string(dir.join("task").join("task.md"))
            .map(|t| crate::experience::entry::title_of(&t))
            .unwrap_or_default();
        Ok(Run {
            engine: self,
            log: JobLog::new(&dir, &job.id),
            job,
            dir,
            task_fingerprint: meta.fingerprint,
            gate: Arc::new(JobGate::new(cap)),
            tripwire: Tripwire::new(&self.config.leak_globs),
            cache: cache::Cache::of(self.profile_dir),
            active,
            summary: Mutex::new(summary),
            model_calls: AtomicU32::new(0),
            rollouts_executed: AtomicU32::new(0),
            library: None,
            title,
        })
    }
}

/// Takes `<job>/run.lock` without waiting.
fn lock(dir: &Path) -> Result<std::fs::File, Error> {
    let path = dir.join("run.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| Error::io(&codes::HARNESS_IO, &path, &e))?;
    pmt_core::file_lock::try_lock(&file).map_err(|_| {
        Error::new(
            &codes::EVOLVE_JOB_RUNNING,
            "this evolution job is already running in another process",
        )
    })?;
    Ok(file)
}

/// Whether another process holds the job's run lock.
#[must_use]
pub fn is_running(dir: &Path) -> bool {
    lock(dir).is_err()
}

/// Cancels the gate once `path` appears (`evolve cancel` from any
/// process).
fn watch_cancel(path: PathBuf, gate: Arc<JobGate>) -> tokio::task::JoinHandle<()> {
    pmt_core::spawn::spawn(async move {
        loop {
            if path.exists() {
                gate.cancel();
                return;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
}

impl Run<'_> {
    async fn execute(&self) -> Result<Finished, Error> {
        Jobs::of(self.engine.profile_dir).set_status(&self.job.id, "running")?;
        self.update(|_| {});
        self.log
            .emit("evolve.job.started", json!({"status": "running"}));
        if let Err(hit) = self.tripwire.check_dir(&self.dir.join("task")) {
            tracing::warn!(job = %self.job.id, "{hit}");
            return self.finish("failed", Some(&hit), Vec::new(), Ledger::default());
        }
        let base = self.base_rollouts().await;
        if let Some(stop) = self.gate.stopped() {
            return self.finish(stop.status(), None, Vec::new(), Ledger::default());
        }
        if !base.iter().any(|b| b.outcome.ok) {
            return self.finish(
                "base-failed",
                Some(BASE_FAILED),
                Vec::new(),
                Ledger::default(),
            );
        }
        let mode =
            pmt_config::schema::EvolveMode::parse(&self.job.mode).unwrap_or(EvolveMode::Parallel);
        let mut ledger = Ledger::default();
        let mut chosen = Vec::new();
        let mut statuses = Vec::new();
        for (i, plan) in budget::plans(mode, self.job.k).iter().enumerate() {
            let (status, pick) = self.mode(plan, &base, i > 0, &mut ledger).await;
            statuses.push(status);
            chosen.extend(pick);
            if self.gate.stopped().is_some() {
                break;
            }
        }
        if let Some(stop) = self.gate.stopped() {
            return self.finish(stop.status(), None, Vec::new(), ledger);
        }
        let status = if statuses.iter().any(|s| s == "awaiting-selection") {
            "awaiting-selection"
        } else if statuses.iter().all(|s| s == "ok") {
            "ok"
        } else {
            "partial"
        };
        if status == "awaiting-selection" {
            return self.finish(status, None, Vec::new(), ledger);
        }
        self.finish(status, None, chosen, ledger)
    }

    async fn base_rollouts(&self) -> Vec<RolloutDone> {
        let labels = budget::plans(EvolveMode::Parallel, self.job.k)
            .remove(0)
            .base;
        let stores: Vec<PathBuf> = labels
            .iter()
            .map(|l| self.dir.join("base").join(l))
            .collect();
        let base = join_all(
            labels
                .iter()
                .zip(&stores)
                .map(|(label, store)| self.rollout(label, &self.active, store)),
        )
        .await;
        self.update(|summary| {
            summary.base = base
                .iter()
                .map(|b| BaseSummary {
                    label: b.label.clone(),
                    ok: b.outcome.ok,
                    error: b.outcome.error.clone(),
                })
                .collect();
        });
        base
    }

    /// Runs one mode and its selection; the status and the chosen final.
    async fn mode(
        &self,
        plan: &ModePlan,
        base: &[RolloutDone],
        shared: bool,
        ledger: &mut Ledger,
    ) -> (String, Option<Pick>) {
        let mut run: ModeRun = if plan.mode == "sequential" {
            self.sequential(plan, base, shared).await
        } else {
            self.parallel(plan, base, shared).await
        };
        let status = if run.stopped.is_some() {
            "stopped".to_owned()
        } else if let Err(violation) = run.ledger.check(self.job.k) {
            violation
        } else {
            String::new()
        };
        let mut pick = None;
        let status = if status.is_empty() {
            let (selection, entry) = self.select(plan.mode, &run.finals).await;
            run.ledger.selection = entry;
            let ids = self.eligible(plan.mode, &run.finals);
            self.update(|s| s.mode(plan.mode).candidates.clone_from(&ids));
            let status = self.selection_status(plan.mode, selection, &run.finals, &mut pick);
            if let Some(mut chosen) = pick.take() {
                let (distilled, entry) = self
                    .reflect(&reflect::Search {
                        mode: plan.mode,
                        base,
                        candidates: &run.candidates,
                        chosen: &chosen.candidate,
                        selected: &chosen.selected,
                        ids: &ids,
                    })
                    .await;
                run.ledger.reflection = entry;
                chosen.distilled = distilled;
                pick = Some(chosen);
            }
            status
        } else {
            status
        };
        let similarity = run.similarity;
        self.update(|s| {
            let mode = s.mode(plan.mode);
            mode.status.clone_from(&status);
            mode.similarity = similarity;
        });
        ledger.modes.push(run.ledger);
        (status, pick)
    }

    fn selection_status(
        &self,
        mode: &str,
        selection: Selection,
        finals: &[Candidate],
        pick: &mut Option<Pick>,
    ) -> String {
        match selection {
            Selection::Chosen(i, selected) => {
                self.log.emit(
                    "evolve.selection.finished",
                    json!({"selected": selected.label}),
                );
                self.update(|s| s.mode(mode).selected = Some(selected.clone()));
                *pick = Some(Pick {
                    mode: mode.to_owned(),
                    candidate: finals[i].clone(),
                    selected,
                    distilled: Distilled::default(),
                });
                "ok".to_owned()
            }
            Selection::Awaiting => "awaiting-selection".to_owned(),
            Selection::Failed(why) => {
                self.log
                    .emit("evolve.selection.finished", json!({"selected": null}));
                tracing::warn!(job = %self.job.id, mode, "{why}");
                "selection failed".to_owned()
            }
        }
    }

    /// Writes the ledger, the proposals of `chosen` and the final summary.
    fn finish(
        &self,
        status: &str,
        error: Option<&str>,
        chosen: Vec<Pick>,
        mut ledger: Ledger,
    ) -> Result<Finished, Error> {
        // Every proposal exists before the policy runs: with mode `both`,
        // adopting one supersedes its siblings on the same base, so at most
        // one is ever adopted.
        let mut created = Vec::new();
        for pick in chosen {
            let (proposal, fresh) = self.propose(&pick)?;
            self.update(|s| s.mode(&pick.mode).proposal_id = Some(proposal.id.clone()));
            created.push((proposal, fresh));
        }
        let mut proposals = Vec::new();
        let mut notices = Vec::new();
        for (proposal, fresh) in created {
            proposals.push(if fresh {
                self.apply_policy(&proposal.id, &mut notices)?
            } else {
                ProposalOutcome {
                    proposal,
                    adopted: None,
                }
            });
        }
        ledger.costs = self.gate.costs();
        ledger.total_usd = ledger.costs.total();
        let path = self.dir.join("ledger.json");
        let bytes = serde_json::to_vec_pretty(&ledger)
            .map_err(|e| Error::new(&codes::HARNESS_IO, e.to_string()))?;
        pmt_core::private_fs::write_atomic(&path, &bytes)
            .map_err(|e| Error::io(&codes::HARNESS_IO, &path, &e))?;
        self.update(|s| {
            status.clone_into(&mut s.status);
            s.error = error.map(str::to_owned);
        });
        Jobs::of(self.engine.profile_dir).set_status(&self.job.id, status)?;
        self.log
            .emit("evolve.job.finished", json!({"status": status}));
        if status == "awaiting-selection" {
            notices.push(format!(
                "job {} awaits your choice: prometheus evolve compare {} then prometheus evolve choose {} <ID>",
                self.job.id, self.job.id, self.job.id
            ));
        }
        let summary = self
            .summary
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        Ok(Finished {
            summary,
            proposals,
            notices,
        })
    }

    /// The proposal of `mode`: the one a resumed job already made
    /// (`false`), else a new one (`true`).
    fn propose(&self, pick: &Pick) -> Result<(proposal::Proposal, bool), Error> {
        let (mode, selected) = (pick.mode.as_str(), &pick.selected);
        let base_version = self
            .summary
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .base_version
            .clone()
            .unwrap_or_default();
        let store = Proposals::of(self.engine.profile_dir);
        if let Some(existing) = store
            .list()?
            .into_iter()
            .find(|p| p.job_id == self.job.id && p.mode == mode)
        {
            return Ok((existing, false));
        }
        let proposal = store.create(&NewProposal {
            job_id: &self.job.id,
            mode,
            label: &selected.label,
            base_version: &base_version,
            base: &self.active,
            tree: &pick.candidate.stage.tree,
            reason: &selected.reason,
            selected_by: &selected.selected_by,
            domain: &self.job.domain,
            title: &self.title,
            k: self.job.k,
            distilled: &pick.distilled,
        })?;
        self.log
            .emit("evolve.proposal.ready", json!({"proposal_id": proposal.id}));
        Ok((proposal, true))
    }

    /// Applies the adoption policy to proposal `id`, re-read so that one
    /// superseded by a sibling's adoption is left alone. A failed adoption
    /// does not fail the job: the proposal stays pending for the user.
    fn apply_policy(&self, id: &str, notices: &mut Vec<String>) -> Result<ProposalOutcome, Error> {
        let store = Proposals::of(self.engine.profile_dir);
        let proposal = store.get(id)?;
        let adopted = match proposal::verdict(self.policy(), &proposal) {
            Verdict::Adopt => self.adopt_now(id, notices),
            verdict => {
                notices.push(self.waiting_notice(&proposal, verdict));
                None
            }
        };
        Ok(ProposalOutcome {
            proposal: store.get(id)?,
            adopted,
        })
    }

    /// The adoption policy of this job: a kanban task's job always waits
    /// for the person who approves the task (spec: kanban, Eval and
    /// harness-evolution tasks), whatever `evolve.adoption.policy` says.
    fn policy(&self) -> pmt_config::schema::AdoptionPolicy {
        if self.job.trigger == crate::job::Trigger::Kanban {
            pmt_config::schema::AdoptionPolicy::Ask
        } else {
            self.engine.config.adoption.policy
        }
    }

    fn adopt_now(&self, id: &str, notices: &mut Vec<String>) -> Option<proposal::Adopted> {
        let settings = self.settings();
        match crate::experience::adopt(
            self.engine.profile_dir,
            id,
            false,
            self.engine.rules,
            &settings,
        ) {
            Ok((adopted, library)) => {
                self.log
                    .emit("evolve.proposal.adopted", json!({"proposal_id": id}));
                notices.push(format!(
                    "proposal {id} adopted as {}; it applies from the next session; {}",
                    adopted.version.id,
                    library.message()
                ));
                Some(adopted)
            }
            Err(e) => {
                tracing::warn!(job = %self.job.id, proposal = id, "automatic adoption failed: {e}");
                notices.push(format!(
                    "proposal {id} could not be adopted automatically ({e}); it stays pending: prometheus evolve review {id}"
                ));
                None
            }
        }
    }

    fn waiting_notice(&self, proposal: &proposal::Proposal, verdict: Verdict) -> String {
        let id = &proposal.id;
        match (verdict, proposal.status) {
            (Verdict::Wait(true), _) => format!(
                "proposal {id} changes scripts, hooks or subagents: approval is required (prometheus evolve review {id}; prometheus evolve adopt {id})"
            ),
            (Verdict::Wait(false), _) => {
                format!("proposal {id} is ready: prometheus evolve review {id}")
            }
            (_, proposal::Status::Superseded) => format!(
                "proposal {id} ({}) is superseded: another proposal of job {} was adopted",
                proposal.mode, self.job.id
            ),
            _ => format!(
                "job {} found no change to propose ({})",
                self.job.id, proposal.mode
            ),
        }
    }
}

/// Removes the directories of finished jobs older than `retention_secs`,
/// keeping `job.toml` and `summary.json` of jobs whose proposal was
/// adopted. Returns the jobs it pruned.
///
/// # Errors
///
/// `harness.io`.
pub fn prune(profile_dir: &Path, retention_secs: u64) -> Result<Vec<String>, Error> {
    const KEPT: &[&str] = &["job.toml", "summary.json"];
    let jobs = Jobs::of(profile_dir);
    let adopted: std::collections::BTreeSet<String> = Proposals::of(profile_dir)
        .list()?
        .into_iter()
        .filter(|p| p.status == proposal::Status::Adopted)
        .map(|p| p.job_id)
        .collect();
    let now = jiff::Timestamp::now();
    let mut pruned = Vec::new();
    for job in jobs.list()? {
        let age = job
            .created_at
            .parse::<jiff::Timestamp>()
            .map_or(0, |t| now.as_second() - t.as_second());
        let unfinished = matches!(
            job.status.as_str(),
            "queued" | "running" | "awaiting-selection"
        );
        if unfinished || age < i64::try_from(retention_secs).unwrap_or(i64::MAX) {
            continue;
        }
        let dir = jobs.dir(&job.id);
        let removed = if adopted.contains(&job.id) {
            strip(&dir, KEPT)?
        } else {
            std::fs::remove_dir_all(&dir).map_err(|e| Error::io(&codes::HARNESS_IO, &dir, &e))?;
            true
        };
        if removed {
            pruned.push(job.id);
        }
    }
    Ok(pruned)
}

/// Removes everything in `dir` but `kept`; whether anything went.
fn strip(dir: &Path, kept: &[&str]) -> Result<bool, Error> {
    let io = |e: std::io::Error| Error::io(&codes::HARNESS_IO, dir, &e);
    let mut removed = false;
    for entry in std::fs::read_dir(dir).map_err(io)?.filter_map(Result::ok) {
        if kept.iter().any(|k| entry.file_name() == *k) {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            std::fs::remove_dir_all(&path).map_err(io)?;
        } else {
            std::fs::remove_file(&path).map_err(io)?;
        }
        removed = true;
    }
    Ok(removed)
}

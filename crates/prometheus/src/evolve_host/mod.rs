//! The evolution engine in this binary (spec: harness-evolution): its
//! ports on the real session factory ([`rollout`], [`improver`]), and
//! what runs jobs — a foreground `prometheus evolve run`, `evolve
//! resume`/`choose`, and the daemon's runner, which resumes interrupted
//! jobs at start and drains queued ones.

pub mod improver;
pub mod rollout;
pub mod rpc;
pub mod session;

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pmt_config::ProfileScope;
use pmt_core::env::LaunchEnv;
use pmt_core::error::Error;
use pmt_evolve::engine::gate::Limits;
use pmt_evolve::engine::{Engine, Finished};
use pmt_evolve::job::{JobRecord, Jobs, budget};
use pmt_store::Store;

/// The wait before an improver session's retry.
const RETRY_DELAY: Duration = Duration::from_secs(10);

/// Runs job `job_id` of `scope` to its end (or until it awaits a
/// selection), then prunes expired jobs.
///
/// # Errors
///
/// As [`Engine::run`]; store errors.
pub async fn run_job(
    scope: &ProfileScope,
    launch: &LaunchEnv,
    job_id: &str,
) -> Result<Finished, Error> {
    let store = Arc::new(Store::open(scope)?);
    let job = Jobs::of(scope.dir()).get(job_id)?;
    if let Err(err) = rollout::backend(&scope.config().evolve.rollout.backend) {
        fail(scope, job_id, err.message());
        return Err(err);
    }
    let rollouts = rollout::HostRollouts {
        scope: scope.clone(),
        launch: launch.clone(),
        store: Arc::clone(&store),
    };
    let improver = improver::HostImprover {
        scope: scope.clone(),
        launch: launch.clone(),
        store,
    };
    let rules = crate::harness_host::rules(scope, launch.user_home().map(PathBuf::as_path));
    let config = &scope.config().evolve;
    let engine = Engine {
        profile_dir: scope.dir(),
        config,
        rules: &rules,
        rollouts: &rollouts,
        improver: &improver,
        limits: Limits::process(config),
        retry_delay: RETRY_DELAY,
        experience: &scope.config().experience,
        estimate_usd: estimate(scope, &job),
    };
    let finished = engine.run(job_id).await;
    match pmt_evolve::engine::prune(scope.dir(), config.retention_secs) {
        Ok(pruned) if !pruned.is_empty() => {
            tracing::info!(?pruned, "expired evolution jobs pruned");
        }
        Ok(_) => {}
        Err(err) => tracing::warn!(error = %err, "evolution jobs not pruned"),
    }
    finished
}

/// Ends job `id` `failed` with `message` before it could run.
fn fail(scope: &ProfileScope, id: &str, message: &str) {
    let jobs = Jobs::of(scope.dir());
    let mut summary =
        pmt_evolve::engine::summary::JobSummary::read(&jobs.dir(id)).unwrap_or_default();
    id.clone_into(&mut summary.job_id);
    "failed".clone_into(&mut summary.status);
    summary.error = Some(message.to_owned());
    if let Err(err) = summary
        .write(&jobs.dir(id))
        .and_then(|()| jobs.set_status(id, "failed"))
    {
        tracing::warn!(job = id, error = %err, "failed job not recorded");
    }
}

/// The spend estimate of `job`: its rollouts and sessions at the mean
/// cost of each in the profile's earlier jobs (0 without history).
#[must_use]
pub fn estimate(scope: &ProfileScope, job: &JobRecord) -> f64 {
    let jobs = Jobs::of(scope.dir());
    let (mut usd, mut units) = (0.0, 0_u32);
    for earlier in jobs.list().unwrap_or_default() {
        let Ok(text) = std::fs::read_to_string(jobs.dir(&earlier.id).join("ledger.json")) else {
            continue;
        };
        let Ok(ledger) = serde_json::from_str::<pmt_evolve::engine::ledger::Ledger>(&text) else {
            continue;
        };
        for entry in ledger
            .modes
            .iter()
            .flat_map(|m| &m.entries)
            .filter(|e| !e.cached)
        {
            usd += entry.usd;
            units += 1;
        }
    }
    if units == 0 {
        return 0.0;
    }
    let mode = pmt_config::schema::EvolveMode::parse(&job.mode)
        .unwrap_or(pmt_config::schema::EvolveMode::Parallel);
    let planned: usize = budget::plans(mode, job.k)
        .iter()
        .map(|p| p.rollouts() + p.sessions())
        .sum();
    usd / f64::from(units) * f64::from(u32::try_from(planned).unwrap_or(u32::MAX))
}

/// The queued jobs to start now: per domain, the oldest queued one, and
/// only while no job of that domain runs, so a domain's jobs run strictly
/// in queue order (spec: experience-library, Stream order within a
/// domain). `running` holds the jobs some process runs now. A job still
/// to be classified (no domain yet) is its own stream until the
/// classifier files it.
#[must_use]
pub fn next_per_domain(jobs: &[JobRecord], running: &BTreeSet<String>) -> Vec<String> {
    let mut busy: BTreeSet<&str> = jobs
        .iter()
        .filter(|j| running.contains(&j.id) && !j.domain.is_empty())
        .map(|j| j.domain.as_str())
        .collect();
    let mut next = Vec::new();
    for job in jobs
        .iter()
        .filter(|j| j.status == "queued" && !running.contains(&j.id))
    {
        if job.domain.is_empty() || busy.insert(job.domain.as_str()) {
            next.push(job.id.clone());
        }
    }
    next
}

/// The daemon's runner of one profile: interrupted jobs are queued again
/// once (`evolve.resume_on_start`), then queued jobs start as they come,
/// each once per process.
#[derive(Default)]
pub struct Runner {
    resumed: Mutex<BTreeSet<PathBuf>>,
    running: Arc<Mutex<BTreeSet<String>>>,
}

impl Runner {
    /// One pass over `scope`.
    pub fn tick(&self, scope: &ProfileScope, launch: &LaunchEnv) {
        let config = &scope.config().evolve;
        if !config.enabled {
            return;
        }
        let jobs = Jobs::of(scope.dir());
        let first = self
            .resumed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(scope.dir().to_path_buf());
        if first && config.resume_on_start {
            match jobs.requeue_interrupted() {
                Ok(ids) if !ids.is_empty() => {
                    tracing::info!(?ids, "interrupted evolution jobs queued again");
                }
                Ok(_) => {}
                Err(err) => tracing::warn!(error = %err, "interrupted evolution jobs not requeued"),
            }
        }
        let listed = jobs.list().unwrap_or_default();
        let mut active = self.running();
        active.extend(
            listed
                .iter()
                .filter(|j| {
                    j.status == "running" && pmt_evolve::engine::is_running(&jobs.dir(&j.id))
                })
                .map(|j| j.id.clone()),
        );
        for id in next_per_domain(&listed, &active) {
            let fresh = self
                .running
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(id.clone());
            if fresh {
                self.start(scope.clone(), launch.clone(), id);
            }
        }
    }

    fn running(&self) -> BTreeSet<String> {
        self.running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn start(&self, scope: ProfileScope, launch: LaunchEnv, id: String) {
        let running = Arc::clone(&self.running);
        pmt_core::spawn::spawn(async move {
            match Box::pin(run_job(&scope, &launch, &id)).await {
                Ok(done) => {
                    tracing::info!(job = %id, status = %done.summary.status, "evolution job ended");
                }
                Err(err) => tracing::warn!(job = %id, error = %err, "evolution job did not run"),
            }
            running
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&id);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(id: &str, status: &str, domain: &str) -> JobRecord {
        JobRecord {
            id: id.to_owned(),
            status: status.to_owned(),
            trigger: pmt_evolve::job::Trigger::Cli,
            cron_job_id: None,
            kanban_task_id: None,
            source_session: None,
            mode: "parallel".to_owned(),
            k: 1,
            domain: domain.to_owned(),
            budget_usd: None,
            created_at: String::new(),
            plan: Vec::new(),
        }
    }

    /// Scenario: Second job sees the first (the runner never starts a
    /// domain's next job while one of it runs).
    #[test]
    fn one_job_per_domain_starts_in_queue_order() {
        let jobs = [
            job("evo_1", "running", "accounting"),
            job("evo_2", "queued", "accounting"),
            job("evo_3", "queued", "devops"),
            job("evo_4", "queued", "devops"),
            job("evo_5", "queued", ""),
        ];
        let running: BTreeSet<String> = ["evo_1".to_owned()].into();
        assert_eq!(next_per_domain(&jobs, &running), ["evo_3", "evo_5"]);
        assert_eq!(
            next_per_domain(&jobs, &BTreeSet::new()),
            ["evo_2", "evo_3", "evo_5"],
            "a job no process runs does not hold its domain"
        );
    }
}

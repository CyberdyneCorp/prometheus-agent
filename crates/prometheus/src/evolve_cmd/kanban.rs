//! Kanban `evolve` tasks (spec: kanban, Eval and harness-evolution tasks;
//! harness-evolution, On-demand and scheduled triggers): the worker
//! queues a job with `trigger = "kanban"` and the task's id, runs it to
//! its end in the foreground, and hands the selected proposal to review
//! with its diff attached. The job never adopts by itself (its policy is
//! always `ask`); approving the task adopts the proposal, from the
//! profile's next session.

use std::path::{Path, PathBuf};

use pmt_config::ProfileScope;
use pmt_config::harness::HarnessDir;
use pmt_config::schema::EvolveMode;
use pmt_core::env::LaunchEnv;
use pmt_core::error::{Error, codes};
use pmt_core::id::SessionId;
use pmt_evolve::harness::diff;
use pmt_evolve::job::{self, Jobs, Request, TaskInput, Trigger};
use pmt_evolve::proposal::{Proposal, Proposals, Status};
use pmt_kanban::{Task, TaskKind};

/// Queues the job of evolve task `task` in its assignee's profile; the
/// job id. `target_task` is a session id, else a task file relative to
/// the task's workspace.
///
/// # Errors
///
/// `surface.not_included`, `evolve.disabled`, `cli.usage` (k),
/// `evolve.invalid_task`.
pub fn queue(scope: &ProfileScope, task: &Task, workspace: &Path) -> Result<String, Error> {
    crate::surface::require("evolve")?;
    let TaskKind::Evolve {
        target_task,
        mode,
        budget_k,
    } = &task.kind
    else {
        unreachable!("only evolve tasks queue evolution jobs");
    };
    let store = pmt_store::Store::open(scope)?;
    let input = match SessionId::parse(target_task) {
        Ok(id) => TaskInput::Session(id),
        Err(_) => TaskInput::File(workspace.join(target_task)),
    };
    let request = Request {
        k: Some(u8::try_from(*budget_k).unwrap_or(u8::MAX)),
        mode: Some(match mode {
            pmt_kanban::model::EvolveMode::Parallel => EvolveMode::Parallel,
            pmt_kanban::model::EvolveMode::Sequential => EvolveMode::Sequential,
        }),
        kanban_task_id: Some(task.id.clone()),
        trigger_domain: task.domain.clone(),
        ..Request::new(Trigger::Kanban, input)
    };
    job::queue(&super::profile(scope, Some(&store)), &request).map(|j| j.id)
}

/// What an evolve task's job produced for review.
#[derive(Debug)]
pub struct Evolved {
    /// The job.
    pub job: String,
    /// The task's result line.
    pub summary: String,
    /// The selected harness's unified diff against its base (empty when
    /// nothing changed).
    pub diff: String,
}

/// The job of `task` (newest first) and its proposals.
fn proposals_of(scope: &ProfileScope, task: &str) -> Result<Vec<Proposal>, Error> {
    let jobs: Vec<String> = Jobs::of(scope.dir())
        .list()?
        .into_iter()
        .filter(|j| j.kanban_task_id.as_deref() == Some(task))
        .map(|j| j.id)
        .collect();
    Ok(Proposals::of(scope.dir())
        .list()?
        .into_iter()
        .filter(|p| jobs.contains(&p.job_id))
        .collect())
}

/// Queues and runs the job of evolve task `task` to its end.
///
/// # Errors
///
/// Queueing errors; `evolve.job_state` when the job does not finish
/// `ok` (failed, cancelled, over budget).
pub async fn run(
    scope: &ProfileScope,
    launch: &LaunchEnv,
    task: &Task,
    workspace: &Path,
) -> Result<Evolved, Error> {
    let job = queue(scope, task, workspace)?;
    let finished = crate::evolve_host::run_job(scope, launch, &job).await?;
    if finished.summary.status != "ok" {
        return Err(Error::new(
            &codes::EVOLVE_JOB_STATE,
            format!(
                "evolution job {job} ended {}: {}",
                finished.summary.status,
                finished.summary.error.as_deref().unwrap_or("no proposal")
            ),
        ));
    }
    let chosen = finished
        .proposals
        .iter()
        .map(|p| &p.proposal)
        .find(|p| p.status == Status::Pending)
        .or_else(|| finished.proposals.first().map(|p| &p.proposal));
    let Some(proposal) = chosen else {
        return Ok(Evolved {
            summary: format!("evolution job {job} proposed no harness"),
            job,
            diff: String::new(),
        });
    };
    let tree = Proposals::of(scope.dir()).tree(&proposal.id)?;
    let base = HarnessDir::of(scope.dir()).tree(&proposal.base_version)?;
    let unified = diff::unified(&base, &tree, (&proposal.base_version, &proposal.id));
    let summary = if proposal.status == Status::NoChange {
        format!(
            "evolution job {job}: proposal {} changes nothing ({})",
            proposal.id, proposal.diff_summary
        )
    } else {
        format!(
            "evolution job {job}: proposal {} on {} ({}); approve this task to adopt it from the next session",
            proposal.id, proposal.base_version, proposal.diff_summary
        )
    };
    Ok(Evolved {
        job,
        summary,
        diff: unified,
    })
}

/// Adopts the pending proposal of evolve task `task` in its assignee's
/// profile (the person approving the task is the approval), effective
/// from the profile's next session; `None` when its job proposed no
/// change.
///
/// # Errors
///
/// `evolve.proposal_refused` (65) when the proposal is stale or invalid,
/// so the task stays in review; store errors.
pub fn adopt(
    scope: &ProfileScope,
    user_home: Option<&Path>,
    task: &Task,
) -> Result<Option<String>, Error> {
    let proposals = proposals_of(scope, &task.id)?;
    let Some(pending) = proposals.iter().find(|p| p.status == Status::Pending) else {
        return Ok(None);
    };
    let rules = crate::harness_host::rules(scope, user_home);
    let settings = crate::experience_cmd::settings(scope);
    let (adopted, library) =
        pmt_evolve::experience::adopt(scope.dir(), &pending.id, false, &rules, &settings)?;
    crate::harness_cmd::record_activation(scope, &adopted.from, &adopted.version);
    let job_dir: PathBuf = Jobs::of(scope.dir()).dir(&pending.job_id);
    if job_dir.is_dir() {
        pmt_evolve::engine::events::JobLog::new(&job_dir, &pending.job_id).emit(
            "evolve.proposal.adopted",
            serde_json::json!({"proposal_id": pending.id, "kanban_task_id": task.id}),
        );
    }
    Ok(Some(format!(
        "adopted {} as {} (from {}); it applies from the next session; {}",
        pending.id,
        adopted.version.id,
        adopted.from,
        library.message()
    )))
}

//! The `evolve.*` and `harness.*` methods in the daemon (spec:
//! harness-evolution, Job lifecycle, events and RPC): each does what the
//! matching subcommand does. A started or chosen job is left `queued`
//! for the daemon's evolution runner.

use pmt_config::ProfileScope;
use pmt_config::harness::{HarnessDir, Source};
use pmt_core::env::LaunchEnv;
use pmt_core::error::{Error, codes};
use pmt_core::id::SessionId;
use pmt_evolve::engine::summary::JobSummary;
use pmt_evolve::harness::diff;
use pmt_evolve::job::{self, Jobs, Request, TaskInput, Trigger};
use pmt_evolve::proposal::Proposals;
use pmt_gateway::evolve::EvolveRequest;
use pmt_rpc::contracts::EvolveParams;
use serde_json::{Value, json};

fn reply(message: impl Into<String>, data: Value) -> Value {
    let mut reply = serde_json::Map::new();
    reply.insert("message".to_owned(), Value::String(message.into()));
    reply.insert("data".to_owned(), data);
    Value::Object(reply)
}

fn needed<'a>(field: Option<&'a String>, name: &str) -> Result<&'a str, Error> {
    field
        .map(String::as_str)
        .ok_or_else(|| Error::new(&codes::CLI_USAGE, format!("{name} is required")))
}

/// Runs one method for `scope`.
///
/// # Errors
///
/// The matching command's errors.
pub fn handle(
    scope: &ProfileScope,
    launch: &LaunchEnv,
    request: &EvolveRequest,
) -> Result<Value, Error> {
    let p = &request.params;
    if request.method.starts_with("harness.") {
        return harness(scope, request.method, p);
    }
    pmt_evolve::gate::require_enabled(scope.config(), scope.name().as_str())?;
    let jobs = Jobs::of(scope.dir());
    let proposals = Proposals::of(scope.dir());
    match request.method {
        "evolve.start" => start(scope, p),
        "evolve.status" => {
            let id = needed(p.job_id.as_ref(), "job_id")?;
            let record = jobs.get(id)?;
            let summary = JobSummary::read(&jobs.dir(id)).unwrap_or_default();
            let candidates = candidates(scope, &jobs.dir(id), &summary);
            let proposals = job_proposals(scope, &proposals, id)?;
            Ok(reply(
                format!("job {id}: {}", record.status),
                json!({"job": record, "summary": summary, "candidates": candidates, "proposals": proposals}),
            ))
        }
        "evolve.list" => list(&jobs),
        "evolve.cancel" => {
            let id = needed(p.job_id.as_ref(), "job_id")?;
            let running = jobs.cancel(id)?;
            Ok(reply(
                format!("job {id} cancelled"),
                json!({"running": running}),
            ))
        }
        "evolve.choose" => {
            let id = needed(p.job_id.as_ref(), "job_id")?;
            let mode = jobs.choose(id, needed(p.candidate.as_ref(), "candidate")?)?;
            Ok(reply(
                format!("job {id}: {mode} candidate chosen; it continues"),
                json!({"mode": mode}),
            ))
        }
        "evolve.proposals.list" => Ok(json!({"proposals": proposals.list()?})),
        "evolve.proposals.adopt" => adopt(scope, launch, p),
        "evolve.proposals.reject" => {
            let id = needed(p.proposal_id.as_ref(), "proposal_id")?;
            let proposal = proposals.reject(id)?;
            Ok(reply(
                format!("rejected {id}"),
                serde_json::to_value(proposal).unwrap_or_default(),
            ))
        }
        other => Err(Error::new(
            &codes::RPC_NOT_SERVED,
            format!("{other} is not served"),
        )),
    }
}

fn start(scope: &ProfileScope, p: &EvolveParams) -> Result<Value, Error> {
    let (task, store) = match (&p.session_id, &p.task_file) {
        (Some(id), _) => (
            TaskInput::Session(SessionId::parse(id)?),
            Some(pmt_store::Store::open(scope)?),
        ),
        (None, Some(file)) => (TaskInput::File(file.into()), None),
        (None, None) => {
            return Err(Error::new(
                &codes::CLI_USAGE,
                "session_id or task_file is required",
            ));
        }
    };
    let mode = match p.mode.as_deref() {
        Some(name) => Some(
            pmt_config::schema::EvolveMode::parse(name)
                .ok_or_else(|| Error::new(&codes::CLI_USAGE, format!("unknown mode '{name}'")))?,
        ),
        None => None,
    };
    let request = Request {
        k: p.k,
        mode,
        domain: p.domain.clone(),
        budget_usd: p.budget_usd,
        ..Request::new(Trigger::Cli, task)
    };
    let record = job::queue(&crate::evolve_cmd::profile(scope, store.as_ref()), &request)?;
    Ok(reply(
        crate::evolve_cmd::queued_line(&record),
        serde_json::to_value(&record).unwrap_or_default(),
    ))
}

fn list(jobs: &Jobs) -> Result<Value, Error> {
    let rows: Vec<Value> = jobs
        .list()?
        .into_iter()
        .map(|j| {
            let cost = JobSummary::read(&jobs.dir(&j.id)).map_or(0.0, |s| s.costs.total());
            json!({
                "id": j.id,
                "status": j.status,
                "trigger": j.trigger.as_str(),
                "mode": j.mode,
                "k": j.k,
                "budget": j.budget_usd,
                "rollouts": j.plan.iter().map(|p| p.rollouts).sum::<usize>(),
                "sessions": j.plan.iter().map(|p| p.sessions).sum::<usize>(),
                "cost_usd": cost,
                "domain": j.domain,
                "created_at": j.created_at,
            })
        })
        .collect();
    Ok(json!({"jobs": rows}))
}

/// Every candidate harness a job's modes produced (`modes/<mode>/<label>/
/// harness`), with its diff against the job's base version: what the
/// dashboard's run detail shows.
fn candidates(scope: &ProfileScope, job_dir: &std::path::Path, summary: &JobSummary) -> Vec<Value> {
    let harness = HarnessDir::of(scope.dir());
    let Some(base_id) = summary.base_version.as_deref() else {
        return Vec::new();
    };
    let Ok(base) = harness.tree(base_id) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for mode in &summary.modes {
        for stage in &mode.stages {
            let dir = job_dir
                .join("modes")
                .join(&mode.mode)
                .join(&stage.label)
                .join("harness");
            let Ok((tree, _)) = pmt_config::harness::Tree::read(&dir) else {
                continue;
            };
            out.push(json!({
                "mode": mode.mode,
                "label": stage.label,
                "summary": diff::summary(&base, &tree).to_string(),
                "diff": diff::unified(&base, &tree, (base_id, &stage.label)),
            }));
        }
    }
    out
}

/// The proposals of job `id`, each with its diff against its base.
fn job_proposals(
    scope: &ProfileScope,
    proposals: &Proposals,
    id: &str,
) -> Result<Vec<Value>, Error> {
    let harness = HarnessDir::of(scope.dir());
    let mut out = Vec::new();
    for proposal in proposals.list()?.into_iter().filter(|p| p.job_id == id) {
        let unified = match (
            harness.tree(&proposal.base_version),
            proposals.tree(&proposal.id),
        ) {
            (Ok(base), Ok(tree)) => {
                diff::unified(&base, &tree, (&proposal.base_version, &proposal.id))
            }
            _ => String::new(),
        };
        let mut row = serde_json::to_value(&proposal).unwrap_or_default();
        row["diff"] = Value::String(unified);
        out.push(row);
    }
    Ok(out)
}

fn adopt(scope: &ProfileScope, launch: &LaunchEnv, p: &EvolveParams) -> Result<Value, Error> {
    let id = needed(p.proposal_id.as_ref(), "proposal_id")?;
    let rules =
        crate::harness_host::rules(scope, launch.user_home().map(std::path::PathBuf::as_path));
    let settings = crate::experience_cmd::settings(scope);
    let (adopted, library) =
        pmt_evolve::experience::adopt(scope.dir(), id, p.rebase, &rules, &settings)?;
    crate::harness_cmd::record_activation(scope, &adopted.from, &adopted.version);
    let job_dir = Jobs::of(scope.dir()).dir(&adopted.proposal.job_id);
    if job_dir.is_dir() {
        pmt_evolve::engine::events::JobLog::new(&job_dir, &adopted.proposal.job_id)
            .emit("evolve.proposal.adopted", json!({"proposal_id": id}));
    }
    let rebuilt = activate_now(scope, &adopted.version.id, p.now)?;
    Ok(reply(
        format!(
            "adopted {id} as {}; {}",
            adopted.version.id,
            library.message()
        ),
        json!({"from": adopted.from, "to": adopted.version.id, "superseded": adopted.superseded, "rebuilt_sessions": rebuilt, "experience": library.message()}),
    ))
}

fn activate_now(scope: &ProfileScope, version: &str, now: bool) -> Result<Option<usize>, Error> {
    if !now {
        return Ok(None);
    }
    let store = pmt_store::Store::open(scope)?;
    crate::harness_host::now::activate(scope, &store, version).map(Some)
}

fn harness(scope: &ProfileScope, method: &str, p: &EvolveParams) -> Result<Value, Error> {
    let harness = HarnessDir::of(scope.dir());
    match method {
        "harness.log" => {
            Ok(json!({"active": harness.active_id()?, "versions": harness.versions()?}))
        }
        "harness.diff" => {
            let from = harness.resolve(needed(p.version.as_ref(), "version")?)?;
            let to = match &p.other {
                Some(other) => harness.resolve(other)?,
                None => harness.active()?,
            };
            let (old, new) = (harness.tree(&from.id)?, harness.tree(&to.id)?);
            let summary = diff::summary(&old, &new);
            Ok(reply(
                summary.to_string(),
                json!({"from": from.id, "to": to.id, "summary": summary, "diff": diff::unified(&old, &new, (&from.id, &to.id))}),
            ))
        }
        "harness.rollback" => {
            let target = harness.resolve(needed(p.version.as_ref(), "version")?)?;
            let tree = harness.tree(&target.id)?;
            let from = harness.active_id()?;
            let written = harness.write(&tree, Source::Rollback, None)?;
            harness.activate(&written.id)?;
            crate::harness_cmd::record_activation(scope, &from, &written);
            let demoted = crate::harness_cmd::demote_entries(scope);
            let rebuilt = activate_now(scope, &written.id, p.now)?;
            Ok(reply(
                format!("activated {} (rollback of {})", written.id, target.id),
                json!({"from": from, "to": written.id, "rebuilt_sessions": rebuilt, "demoted_entries": demoted}),
            ))
        }
        other => Err(Error::new(
            &codes::RPC_NOT_SERVED,
            format!("{other} is not served"),
        )),
    }
}

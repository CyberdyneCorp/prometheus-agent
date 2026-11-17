//! Running and inspecting jobs (spec: harness-evolution, Job lifecycle,
//! events and RPC; User-driven selection; Resume and content-addressed
//! session cache): the foreground run (exit 0 when the job ends `ok`,
//! else 1), `status`, `cancel`, `resume`, `compare` and `choose`.

use std::fmt::Write as _;

use pmt_config::ProfileScope;
use pmt_core::error::Error;
use pmt_evolve::engine::Finished;
use pmt_evolve::engine::summary::JobSummary;
use pmt_evolve::job::Jobs;
use serde_json::json;

use crate::context::Context;
use crate::output::{json, print_raw};

/// Runs job `id` here and reports how it ended.
///
/// # Errors
///
/// As [`crate::evolve_host::run_job`].
pub fn foreground(ctx: &Context<'_>, scope: &ProfileScope, id: &str) -> Result<u8, Error> {
    let job = Jobs::of(scope.dir()).get(id)?;
    if !ctx.json {
        let estimate = crate::evolve_host::estimate(scope, &job);
        let cap = job
            .budget_usd
            .unwrap_or(scope.config().evolve.budget.max_usd);
        print_raw(&format!("estimate: ${estimate:.2} (cap ${cap:.2})\n"))?;
    }
    let finished =
        match crate::model_cmd::block_on(crate::evolve_host::run_job(scope, ctx.launch, id))? {
            // The daemon's runner took the job first: follow it to its end.
            Err(err) if err.code().code == pmt_core::error::codes::EVOLVE_JOB_RUNNING.code => {
                followed(scope, id)
            }
            other => other?,
        };
    finish(ctx, &finished)
}

/// Waits until another process finishes job `id`; its summary.
fn followed(scope: &ProfileScope, id: &str) -> Finished {
    let dir = Jobs::of(scope.dir()).dir(id);
    while pmt_evolve::engine::is_running(&dir) {
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    Finished {
        summary: JobSummary::read(&dir).unwrap_or_default(),
        proposals: Vec::new(),
        notices: Vec::new(),
    }
}

fn finish(ctx: &Context<'_>, finished: &Finished) -> Result<u8, Error> {
    let summary = &finished.summary;
    let code = if summary.status == "ok" {
        pmt_core::exit::SUCCESS
    } else {
        pmt_core::exit::FAILURE
    };
    if ctx.json {
        let proposals: Vec<_> = finished.proposals.iter().map(|p| &p.proposal).collect();
        json(&json!({"summary": summary, "proposals": proposals, "notices": finished.notices}))?;
        return Ok(code);
    }
    let mut text = format!("job {} {}", summary.job_id, summary.status);
    if let Some(error) = &summary.error {
        let _ = write!(text, ": {error}");
    }
    let _ = writeln!(
        text,
        " ({} model calls, {} rollouts, ${:.4})",
        summary.model_calls,
        summary.rollouts_run,
        summary.costs.total()
    );
    for notice in &finished.notices {
        let _ = writeln!(text, "{notice}");
    }
    print_raw(&text)?;
    Ok(code)
}

fn summary_of(scope: &ProfileScope, id: &str) -> Result<JobSummary, Error> {
    let jobs = Jobs::of(scope.dir());
    let record = jobs.get(id)?;
    Ok(
        JobSummary::read(&jobs.dir(id)).unwrap_or_else(|| JobSummary {
            job_id: record.id,
            status: record.status,
            ..JobSummary::default()
        }),
    )
}

/// `evolve status <job>`.
///
/// # Errors
///
/// `evolve.job_not_found`.
pub fn status(ctx: &Context<'_>, scope: &ProfileScope, id: &str) -> Result<u8, Error> {
    let summary = summary_of(scope, id)?;
    if ctx.json {
        return json(&summary);
    }
    print_raw(&render_status(&summary))
}

/// The text of `evolve status`.
#[must_use]
pub fn render_status(summary: &JobSummary) -> String {
    let mut text = format!("job {}: {}", summary.job_id, summary.status);
    if let Some(error) = &summary.error {
        let _ = write!(text, " ({error})");
    }
    text.push('\n');
    if let Some(base) = &summary.base_version {
        let _ = writeln!(text, "base harness: {base}");
    }
    for base in &summary.base {
        let state = if base.ok { "ok" } else { "failed" };
        let _ = writeln!(text, "  {} {state}", base.label);
    }
    for mode in &summary.modes {
        text.push_str(&render_mode(mode));
    }
    let costs = &summary.costs;
    let _ = writeln!(
        text,
        "spend: rollouts ${:.4}, improver ${:.4}, selection ${:.4}, reflection ${:.4}, total ${:.4}",
        costs.rollout,
        costs.improver,
        costs.selection,
        costs.reflection,
        costs.total()
    );
    text
}

fn render_mode(mode: &pmt_evolve::engine::summary::ModeSummary) -> String {
    let mut text = format!("{}: {}\n", mode.mode, mode.status);
    for stage in &mode.stages {
        let state = if stage.fell_back {
            "fell back"
        } else {
            "accepted"
        };
        let cached = if stage.cached { ", cached" } else { "" };
        let error = stage
            .error
            .as_deref()
            .map(|e| format!(": {e}"))
            .unwrap_or_default();
        let _ = writeln!(
            text,
            "  {:<5} {state}, {} attempt(s), ${:.4}{cached}{error}",
            stage.label, stage.attempts, stage.usd
        );
    }
    if let Some(selected) = &mode.selected {
        let reason = if selected.reason.is_empty() {
            String::new()
        } else {
            format!(": {}", selected.reason)
        };
        let _ = writeln!(
            text,
            "  selected {} ({}) by {}{reason}",
            selected.id, selected.label, selected.selected_by
        );
    }
    if let Some(proposal) = &mode.proposal_id {
        let _ = writeln!(text, "  proposal {proposal}");
    }
    text
}

/// `evolve cancel <job>`.
///
/// # Errors
///
/// `evolve.job_not_found`, `evolve.job_state`.
pub fn cancel(ctx: &Context<'_>, scope: &ProfileScope, id: &str) -> Result<u8, Error> {
    let running = Jobs::of(scope.dir()).cancel(id)?;
    if ctx.json {
        return json(&json!({"job_id": id, "running": running}));
    }
    print_raw(&if running {
        format!("job {id} stops at its next model-call boundary\n")
    } else {
        format!("job {id} cancelled\n")
    })
}

/// `evolve resume <job>`.
///
/// # Errors
///
/// `evolve.job_not_found`, `evolve.job_state`; as [`foreground`].
pub fn resume(ctx: &Context<'_>, scope: &ProfileScope, id: &str) -> Result<u8, Error> {
    Jobs::of(scope.dir()).resume(id)?;
    foreground(ctx, scope, id)
}

/// `evolve compare <job>`: each candidate's anonymous view.
///
/// # Errors
///
/// `evolve.job_not_found`, `evolve.job_state` when no candidates exist.
pub fn compare(ctx: &Context<'_>, scope: &ProfileScope, id: &str) -> Result<u8, Error> {
    let summary = summary_of(scope, id)?;
    let dir = Jobs::of(scope.dir()).dir(id);
    let mut views = Vec::new();
    for mode in &summary.modes {
        for (_, candidate) in &mode.candidates {
            let path = dir
                .join("modes")
                .join(&mode.mode)
                .join("candidates")
                .join(format!("{candidate}.md"));
            if let Ok(text) = std::fs::read_to_string(path) {
                views.push((mode.mode.clone(), candidate.clone(), text));
            }
        }
    }
    if views.is_empty() {
        return Err(Error::new(
            &pmt_core::error::codes::EVOLVE_JOB_STATE,
            format!("job {id} has no candidates to compare ({})", summary.status),
        ));
    }
    views.sort();
    if ctx.json {
        let items: Vec<_> = views
            .iter()
            .map(|(mode, id, text)| json!({"mode": mode, "id": id, "view": text}))
            .collect();
        return json(&json!({"job_id": id, "status": summary.status, "candidates": items}));
    }
    let mut text = String::new();
    for (mode, _, view) in &views {
        let _ = writeln!(text, "=== {mode} ===\n{}", view.trim_end());
    }
    print_raw(&text)
}

/// `evolve choose <job> <ID>`: records the pick and continues the job.
///
/// # Errors
///
/// `evolve.job_not_found`, `evolve.job_state`; as [`foreground`].
pub fn choose(
    ctx: &Context<'_>,
    scope: &ProfileScope,
    id: &str,
    candidate: &str,
) -> Result<u8, Error> {
    let mode = Jobs::of(scope.dir()).choose(id, candidate)?;
    if !ctx.json {
        print_raw(&format!("job {id}: {mode} candidate {candidate} chosen\n"))?;
    }
    foreground(ctx, scope, id)
}

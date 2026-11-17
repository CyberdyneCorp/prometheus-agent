//! The self-improvement task kinds a worker runs without an agent session
//! of its own (spec: kanban, Eval and harness-evolution tasks).
//!
//! An `eval` task runs its suite through the eval engine for the assignee
//! profile (`prometheus eval run`, then `eval judge`), attaches the report
//! and completes with the suite's summary. `provider_mode = "replay"`
//! plays every task on the deterministic mock provider (the suite's
//! recorded per-task scripts) with the model-call cache read but never
//! written, so no live provider request is made; `live` (the default)
//! uses the profile's model route.
//!
//! An `evolve` task runs a harness-evolution job to its end and completes
//! into `review` with the selected harness's diff attached; nothing is
//! adopted until a person approves the task.

use std::path::Path;

use pmt_config::ProfileScope;
use pmt_core::env::LaunchEnv;
use pmt_core::error::Error;
use pmt_eval::report::{self, RunInput};
use pmt_kanban::model::ProviderMode;
use pmt_kanban::{Board, Completion, Run, Task};

use super::worker::worker_actor;
use crate::eval_cmd::{CacheFlag, CommonArgs, RunArgs};

/// The `eval run` an eval task makes.
fn run_args(suite: &str, mode: Option<ProviderMode>) -> RunArgs {
    let replay = mode == Some(ProviderMode::Replay);
    RunArgs {
        suite: suite.to_owned(),
        harness: None,
        tasks: Vec::new(),
        repeat: 1,
        common: CommonArgs {
            provider: replay.then(|| pmt_providers::mock::PROVIDER_ID.to_owned()),
            model: None,
            parallel: 4,
            cache: if replay {
                CacheFlag::Read
            } else {
                CacheFlag::ReadWrite
            },
            budget_usd: None,
            run_id: Some(crate::eval_cmd::new_run_id()),
            toolset_distribution: None,
            seed: 0,
        },
    }
}

/// Runs and judges the suite; the report text and the result line.
fn evaluate(
    scope: &ProfileScope,
    launch: &LaunchEnv,
    suite: &str,
    mode: Option<ProviderMode>,
) -> Result<(String, String), Error> {
    let quiet = |_: &str| {};
    let ran =
        crate::eval_cmd::run::execute_run(scope, launch, &run_args(suite, mode), None, &quiet)?;
    let (judged, _) = crate::eval_cmd::judge::judge_run(scope, launch, &ran.info.run_id, None, 8)?;
    let built = report::build(&[RunInput {
        info: ran.info.clone(),
        results: ran.run.results(),
        judged: Some(judged),
    }]);
    let stats = built.runs.first();
    let scored = stats
        .and_then(|s| s.score)
        .map_or_else(|| "ungraded".to_owned(), |s| format!("score {s:.2}"));
    let (n, ungraded) = stats.map_or((0, 0), |s| (s.n, s.n_ungraded));
    let mode = match mode {
        Some(ProviderMode::Replay) => "replay",
        _ => "live",
    };
    let summary = format!(
        "{suite}: {scored} over {} graded of {n} tasks, run {} ({mode} provider)",
        n - ungraded,
        ran.info.run_id
    );
    let text = format!("{summary}\n\n{}", report::render(&built, false));
    Ok((text, summary))
}

/// An `eval` task: run, attach `eval-report.txt` (under `limit`), and
/// complete with the summary.
///
/// # Errors
///
/// Suite, run and judge errors (the worker's exit leaves the run to the
/// dispatcher); attachment and completion refusals.
pub(super) fn eval(
    (scope, launch): (&ProfileScope, &LaunchEnv),
    board: &mut Board,
    (row, task): (&Run, &Task),
    (suite, mode): (&str, Option<ProviderMode>),
    limit: u64,
) -> Result<(), Error> {
    let (text, summary) = evaluate(scope, launch, suite, mode)?;
    finish(
        board,
        (row, task),
        ("eval-report.txt", &text),
        summary,
        limit,
    )
}

/// An `evolve` task: run its job, attach `harness.diff`, complete (into
/// `review`).
///
/// # Errors
///
/// Queueing and job errors; attachment and completion refusals.
pub(super) async fn evolve(
    (scope, launch): (&ProfileScope, &LaunchEnv),
    board: &mut Board,
    (row, task): (&Run, &Task),
    workspace: &Path,
    limit: u64,
) -> Result<(), Error> {
    let evolved = crate::evolve_cmd::kanban::run(scope, launch, task, workspace).await?;
    let diff = if evolved.diff.is_empty() {
        format!("evolution job {}: no harness change\n", evolved.job)
    } else {
        evolved.diff
    };
    finish(
        board,
        (row, task),
        ("harness.diff", &diff),
        evolved.summary,
        limit,
    )
}

fn finish(
    board: &mut Board,
    (row, task): (&Run, &Task),
    (name, text): (&str, &str),
    summary: String,
    limit: u64,
) -> Result<(), Error> {
    let actor = worker_actor(row);
    let now = pmt_kanban::now_ms();
    let upload = pmt_kanban::attach::Upload {
        name,
        content_type: Some("text/plain"),
        bytes: text.as_bytes(),
    };
    board.attach(&actor, &task.id, &upload, limit, now)?;
    let completion = Completion {
        result: Some(summary),
        attachment_limit: limit,
        ..Completion::default()
    };
    board
        .complete(&actor, &task.id, &completion, now)
        .map(|_| ())
}

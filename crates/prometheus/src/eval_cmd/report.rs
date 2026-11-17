//! `prometheus eval report` (spec: replay-and-evaluation, Eval reports and
//! comparison): the figures of each run and, with two or more, the paired
//! deltas against the first with a bootstrap confidence interval.

use pmt_core::error::Error;
use pmt_eval::judge;
use pmt_eval::report::{self, RunInput};
use pmt_eval::run::{RunDir, read_json};

use super::{ReportArgs, ReportFormat};
use crate::context::Context;

/// Runs `prometheus eval report`.
///
/// # Errors
///
/// `eval.run_not_found` (66) for an unknown run.
pub fn run(args: &ReportArgs, ctx: &Context<'_>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let mut inputs = Vec::new();
    for id in &args.run_ids {
        let run = RunDir::existing(scope.dir(), id)?;
        inputs.push(RunInput {
            info: run.info().unwrap_or_default(),
            results: run.results(),
            judged: read_json(&judge::path(&run.dir, args.judge_namespace.as_deref())),
        });
    }
    let built = report::build(&inputs);
    if ctx.json || args.format == ReportFormat::Json {
        return crate::output::json(&built);
    }
    crate::output::print_raw(&report::render(
        &built,
        args.format == ReportFormat::Markdown,
    ))
}

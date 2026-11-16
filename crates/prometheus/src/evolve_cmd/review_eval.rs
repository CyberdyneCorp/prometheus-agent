//! `evolve review <proposal> --eval <suite>` (spec: harness-evolution,
//! Human-only evaluation of proposals): the suite runs through the eval
//! engine once on the proposal's base version and once on the proposed
//! harness, both runs are judged, and the comparison is shown to the user
//! only.
//!
//! A proposal tree is not a harness version (versions are immutable and
//! never deleted, so writing one per review would pollute the history),
//! so the proposed run materialises the tree as a temporary harness under
//! the review's results directory and its task sessions read their
//! prompt layer, skills, subagents and hooks from there. The verdict is
//! written outside `<profile>/evolve/` (`<profile>/eval-results/`) with
//! the verdict canary, so the tripwire rejects it wherever it might turn
//! up, and it is never stored in a proposal, a harness version or an
//! experience entry. The two eval runs stay under `<profile>/evals/runs/`
//! like any other (`prometheus eval report <base> <proposed>`).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use pmt_config::ProfileScope;
use pmt_config::harness::Tree;
use pmt_core::env::LaunchEnv;
use pmt_core::error::{Error, codes};
use pmt_eval::report::{self, RunInput};
use pmt_eval::run::RunDir;
use pmt_evolve::engine::tripwire::VERDICT_CANARY;
use serde_json::{Value, json};

use crate::eval_cmd::run::{TempHarness, execute_run};
use crate::eval_cmd::{CacheFlag, CommonArgs, RunArgs};

/// Both runs of a review.
#[derive(Debug, Clone)]
pub(super) struct Report {
    suite: String,
    /// The base run and the proposed run, judged and compared.
    comparison: report::Report,
    /// The verdict file.
    pub(super) path: PathBuf,
}

impl Report {
    /// The verdict as JSON (what the file holds).
    #[must_use]
    pub(super) fn json(&self) -> Value {
        json!({
            "canary": VERDICT_CANARY,
            "suite": self.suite,
            "base": self.comparison.runs.first(),
            "proposed": self.comparison.runs.get(1),
            "comparison": self.comparison.comparisons.first(),
            "path": self.path,
        })
    }

    /// The lines `evolve review` prints.
    #[must_use]
    pub(super) fn render(&self) -> String {
        let score = |i: usize| {
            self.comparison
                .runs
                .get(i)
                .and_then(|r| r.score)
                .map_or_else(|| "ungraded".to_owned(), |s| format!("{s:.2}"))
        };
        let mut text = format!(
            "\neval {} (for you only; never shown to evolution):\n  base:     score {}\n  proposed: score {}\n",
            self.suite,
            score(0),
            score(1)
        );
        text.push_str(&report::render(&self.comparison, false));
        let _ = writeln!(text, "  results: {}", self.path.display());
        text
    }
}

/// The eval run of one side: the profile's route, the default cache and
/// parallelism, and a fresh run id.
fn run_args(suite: &str, harness: Option<&str>) -> RunArgs {
    RunArgs {
        suite: suite.to_owned(),
        harness: harness.map(str::to_owned),
        tasks: Vec::new(),
        repeat: 1,
        common: CommonArgs {
            provider: None,
            model: None,
            parallel: 4,
            cache: CacheFlag::ReadWrite,
            budget_usd: None,
            run_id: Some(crate::eval_cmd::new_run_id()),
            toolset_distribution: None,
            seed: 0,
        },
    }
}

/// Judges `run` and returns it as report input.
fn judged(scope: &ProfileScope, launch: &LaunchEnv, run: &RunDir) -> Result<RunInput, Error> {
    let info = run.info().unwrap_or_default();
    let (judged, _) = crate::eval_cmd::judge::judge_run(scope, launch, &info.run_id, None, 8)?;
    Ok(RunInput {
        info,
        results: run.results(),
        judged: Some(judged),
    })
}

/// What a review evaluates.
pub(super) struct Sides<'a> {
    /// The proposal id.
    pub(super) proposal: &'a str,
    /// Its base version id.
    pub(super) base_version: &'a str,
    /// The proposed tree.
    pub(super) proposed: &'a Tree,
}

/// Runs `suite` on the base version and on the proposed tree, judges both
/// and writes the verdict.
///
/// # Errors
///
/// Suite, adapter, harness and run errors; I/O errors.
pub(super) fn run(
    scope: &ProfileScope,
    launch: &LaunchEnv,
    sides: &Sides<'_>,
    suite: &str,
) -> Result<Report, Error> {
    let stamp = jiff::Timestamp::now().as_second();
    let out = scope
        .dir()
        .join("eval-results")
        .join(format!("review-{}-{stamp}", sides.proposal));
    let temp = TempHarness {
        label: format!("proposal:{}", sides.proposal),
        dir: out.join("proposed-harness"),
        base: sides.base_version.to_owned(),
    };
    sides
        .proposed
        .write(&temp.dir)
        .map_err(|e| Error::io(&codes::HARNESS_IO, &temp.dir, &e))?;
    let quiet = |_: &str| {};
    let ran = execute_run(
        scope,
        launch,
        &run_args(suite, Some(sides.base_version)),
        None,
        &quiet,
    )
    .and_then(|base| {
        execute_run(scope, launch, &run_args(suite, None), Some(&temp), &quiet)
            .map(|proposed| (base, proposed))
    });
    // The temporary harness goes whatever happened; the verdict stays.
    let _ = std::fs::remove_dir_all(&temp.dir);
    let (base, proposed) = ran?;
    let inputs = [
        judged(scope, launch, &base.run)?,
        judged(scope, launch, &proposed.run)?,
    ];
    let report = Report {
        suite: suite.to_owned(),
        comparison: report::build(&inputs),
        path: out.join("verdict.json"),
    };
    write(&report.path, &report.json())?;
    Ok(report)
}

fn write(path: &Path, value: &Value) -> Result<(), Error> {
    if let Some(parent) = path.parent() {
        pmt_core::private_fs::create_dir(parent)
            .map_err(|e| Error::io(&codes::OUTPUT_IO, parent, &e))?;
    }
    let text = serde_json::to_string_pretty(value).unwrap_or_default();
    std::fs::write(path, text).map_err(|e| Error::io(&codes::OUTPUT_IO, path, &e))
}

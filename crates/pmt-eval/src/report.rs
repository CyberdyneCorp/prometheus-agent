//! Eval reports and comparison (spec: replay-and-evaluation, Eval reports
//! and comparison): per run the score (mean `frac` × 100), the all-pass
//! percentage, graded and ungraded counts, contaminated tasks, cost,
//! unpriced calls, tokens and wall time; with repeats pass@1, pass@N and
//! each task's standard deviation; with two or more runs the paired
//! per-task deltas against the first run and a 95% bootstrap confidence
//! interval of the score difference (10,000 resamples, seed 0).

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::Serialize;

use crate::judge::{Judged, Row};
use crate::rng::Rng;
use crate::run::{RunInfo, TaskResult};

/// Bootstrap resamples.
pub const RESAMPLES: usize = 10_000;

/// One run's inputs.
#[derive(Debug, Clone)]
pub struct RunInput {
    /// `run.json`.
    pub info: RunInfo,
    /// Every `result.json`.
    pub results: Vec<TaskResult>,
    /// `judged.json`, when judged.
    pub judged: Option<Judged>,
}

/// Repeat statistics.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Repeats {
    /// Repeats per task.
    pub n: u32,
    /// Mean per-task fraction of repeats that passed every criterion.
    pub pass_at_1: f64,
    /// Fraction of tasks with at least one all-pass repeat.
    pub pass_at_n: f64,
    /// Each task's standard deviation of `frac` over its repeats.
    pub task_sd: BTreeMap<String, f64>,
}

/// One run's figures.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RunStats {
    /// The run.
    pub run_id: String,
    /// Its suite.
    pub suite: String,
    /// `provider/model`.
    pub model: String,
    /// The harness ref.
    pub harness: String,
    /// Mean `frac` × 100 over graded rows (two decimals).
    pub score: Option<f64>,
    /// Percentage of graded rows passing every criterion.
    pub all_pass_pct: Option<f64>,
    /// Graded rows.
    pub n: usize,
    /// Ungraded rows (or unjudged tasks).
    pub n_ungraded: usize,
    /// Contaminated tasks.
    pub contaminated: usize,
    /// Total spend.
    pub cost_usd: f64,
    /// Model calls without a price.
    pub unpriced_calls: u64,
    /// Input plus output tokens.
    pub tokens: u64,
    /// Summed task wall time.
    pub wall_secs: f64,
    /// With repeats.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repeats: Option<Repeats>,
}

/// One task's paired delta.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TaskDelta {
    /// The task.
    pub task_id: String,
    /// Its mean `frac` × 100 in the first run.
    pub base: f64,
    /// In this run.
    pub other: f64,
    /// `other − base`.
    pub delta: f64,
}

/// A run against the first run.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Comparison {
    /// The first run.
    pub base: String,
    /// This run.
    pub run_id: String,
    /// Paired per-task deltas (tasks graded in both).
    pub deltas: Vec<TaskDelta>,
    /// Mean paired delta (score difference, this − first).
    pub delta: f64,
    /// 95% bootstrap confidence interval of `delta`.
    pub ci: [f64; 2],
    /// The interval contains 0.
    pub within_noise: bool,
}

/// The whole report.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Report {
    /// Per run.
    pub runs: Vec<RunStats>,
    /// Per run after the first.
    pub comparisons: Vec<Comparison>,
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

#[allow(clippy::cast_precision_loss)]
fn mean(xs: &[f64]) -> Option<f64> {
    (!xs.is_empty()).then(|| xs.iter().sum::<f64>() / xs.len() as f64)
}

#[allow(clippy::cast_precision_loss)]
fn sd(xs: &[f64]) -> f64 {
    let Some(m) = mean(xs) else { return 0.0 };
    (xs.iter().map(|x| (x - m).powi(2)).sum::<f64>() / xs.len() as f64).sqrt()
}

fn graded(judged: Option<&Judged>) -> Vec<&Row> {
    judged
        .map(|j| j.rows.iter().filter(|r| r.graded).collect())
        .unwrap_or_default()
}

/// Mean `frac` per task over its graded repeats.
fn per_task(judged: Option<&Judged>) -> BTreeMap<String, Vec<f64>> {
    let mut map: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for row in graded(judged) {
        map.entry(row.task_id.clone())
            .or_default()
            .push(row.frac.unwrap_or_default());
    }
    map
}

#[allow(clippy::cast_precision_loss)]
fn repeats(input: &RunInput) -> Option<Repeats> {
    if input.info.repeat <= 1 {
        return None;
    }
    let mut passes: BTreeMap<&str, Vec<bool>> = BTreeMap::new();
    for row in graded(input.judged.as_ref()) {
        passes
            .entry(&row.task_id)
            .or_default()
            .push(row.all_pass == Some(true));
    }
    let tasks = passes.len().max(1) as f64;
    let pass_at_1 = passes
        .values()
        .map(|p| p.iter().filter(|x| **x).count() as f64 / p.len() as f64)
        .sum::<f64>()
        / tasks;
    let pass_at_n = passes.values().filter(|p| p.iter().any(|x| *x)).count() as f64 / tasks;
    let task_sd = per_task(input.judged.as_ref())
        .into_iter()
        .map(|(task, fracs)| (task, round2(sd(&fracs) * 100.0)))
        .collect();
    Some(Repeats {
        n: input.info.repeat,
        pass_at_1: round2(pass_at_1 * 100.0),
        pass_at_n: round2(pass_at_n * 100.0),
        task_sd,
    })
}

#[allow(clippy::cast_precision_loss)]
fn stats(input: &RunInput) -> RunStats {
    let rows = graded(input.judged.as_ref());
    let fracs: Vec<f64> = rows.iter().filter_map(|r| r.frac).collect();
    let all_pass = rows.iter().filter(|r| r.all_pass == Some(true)).count();
    let n_ungraded = match &input.judged {
        Some(j) => j.rows.len() - rows.len(),
        None => input.results.len(),
    };
    let r = &input.results;
    RunStats {
        run_id: input.info.run_id.clone(),
        suite: input.info.suite.clone(),
        model: format!("{}/{}", input.info.provider, input.info.model),
        harness: input.info.harness_ref.clone(),
        score: mean(&fracs).map(|m| round2(m * 100.0)),
        all_pass_pct: (!rows.is_empty())
            .then(|| round2(all_pass as f64 * 100.0 / rows.len() as f64)),
        n: rows.len(),
        n_ungraded,
        contaminated: r.iter().filter(|t| t.contaminated).count(),
        cost_usd: r.iter().map(|t| t.cost_usd).sum(),
        unpriced_calls: r.iter().map(|t| t.unpriced_calls).sum(),
        tokens: r.iter().map(|t| t.input_tokens + t.output_tokens).sum(),
        wall_secs: round2(r.iter().map(|t| t.wall_secs).sum()),
        repeats: repeats(input),
    }
}

/// The 2.5th and 97.5th percentiles of the mean of `deltas` over
/// [`RESAMPLES`] resamples with replacement (seed 0).
#[must_use]
pub fn bootstrap_ci(deltas: &[f64]) -> [f64; 2] {
    if deltas.is_empty() {
        return [0.0, 0.0];
    }
    let mut rng = Rng::new(0);
    let mut means: Vec<f64> = (0..RESAMPLES)
        .map(|_| {
            let sample: Vec<f64> = (0..deltas.len())
                .map(|_| deltas[rng.below(deltas.len())])
                .collect();
            mean(&sample).unwrap_or_default()
        })
        .collect();
    means.sort_by(f64::total_cmp);
    let at = |q: f64| {
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let i = ((means.len() - 1) as f64 * q).round() as usize;
        means[i]
    };
    [at(0.025), at(0.975)]
}

fn compare(base: &RunInput, other: &RunInput) -> Comparison {
    let a = per_task(base.judged.as_ref());
    let b = per_task(other.judged.as_ref());
    let deltas: Vec<TaskDelta> = a
        .iter()
        .filter_map(|(task, fa)| {
            let fb = b.get(task)?;
            let (ma, mb) = (mean(fa)? * 100.0, mean(fb)? * 100.0);
            Some(TaskDelta {
                task_id: task.clone(),
                base: round2(ma),
                other: round2(mb),
                delta: round2(mb - ma),
            })
        })
        .collect();
    let values: Vec<f64> = deltas.iter().map(|d| d.delta).collect();
    let ci = bootstrap_ci(&values);
    let ci = [round2(ci[0]), round2(ci[1])];
    Comparison {
        base: base.info.run_id.clone(),
        run_id: other.info.run_id.clone(),
        delta: round2(mean(&values).unwrap_or_default()),
        within_noise: ci[0] <= 0.0 && ci[1] >= 0.0,
        ci,
        deltas,
    }
}

/// The report of `inputs` (the first is the comparison base).
#[must_use]
pub fn build(inputs: &[RunInput]) -> Report {
    Report {
        runs: inputs.iter().map(stats).collect(),
        comparisons: inputs
            .iter()
            .skip(1)
            .map(|other| compare(&inputs[0], other))
            .collect(),
    }
}

fn opt(x: Option<f64>) -> String {
    x.map_or_else(|| "-".to_owned(), |v| format!("{v:.2}"))
}

/// The report as text (`markdown` makes tables).
#[must_use]
pub fn render(report: &Report, markdown: bool) -> String {
    let mut out = String::new();
    for run in &report.runs {
        render_run(&mut out, run, markdown);
    }
    for cmp in &report.comparisons {
        render_comparison(&mut out, cmp, markdown);
    }
    out
}

fn render_run(out: &mut String, run: &RunStats, markdown: bool) {
    let head = if markdown { "## " } else { "" };
    let _ = writeln!(
        out,
        "{head}run {} ({}, {}, harness {})",
        run.run_id, run.suite, run.model, run.harness
    );
    let lines = [
        ("score", opt(run.score)),
        ("all_pass", format!("{}%", opt(run.all_pass_pct))),
        ("n", run.n.to_string()),
        ("n_ungraded", run.n_ungraded.to_string()),
        ("contaminated", run.contaminated.to_string()),
        ("cost", format!("{:.4} USD", run.cost_usd)),
        ("unpriced calls", run.unpriced_calls.to_string()),
        ("tokens", run.tokens.to_string()),
        ("wall time", format!("{:.2} s", run.wall_secs)),
    ];
    if markdown {
        out.push_str("\n| metric | value |\n|---|---|\n");
    }
    for (k, v) in lines {
        let _ = if markdown {
            writeln!(out, "| {k} | {v} |")
        } else {
            writeln!(out, "  {k}: {v}")
        };
    }
    if let Some(r) = &run.repeats {
        let _ = writeln!(
            out,
            "  pass@1: {:.2}%  pass@{}: {:.2}%",
            r.pass_at_1, r.n, r.pass_at_n
        );
        for (task, sd) in &r.task_sd {
            let _ = writeln!(out, "  sd {task}: {sd:.2}");
        }
    }
    out.push('\n');
}

fn render_comparison(out: &mut String, cmp: &Comparison, markdown: bool) {
    let head = if markdown { "## " } else { "" };
    let _ = writeln!(out, "{head}{} vs {}", cmp.run_id, cmp.base);
    for d in &cmp.deltas {
        let _ = writeln!(
            out,
            "  {}: {:.2} -> {:.2} ({:+.2})",
            d.task_id, d.base, d.other, d.delta
        );
    }
    let _ = writeln!(
        out,
        "  score delta {} - {}: {:+.2} (95% CI {:+.2} to {:+.2})",
        cmp.run_id, cmp.base, cmp.delta, cmp.ci[0], cmp.ci[1]
    );
    if cmp.within_noise {
        out.push_str("  NOTE: difference within noise\n");
    }
    out.push('\n');
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::judge::{JudgeIdentity, Row};

    fn input(id: &str, repeat: u32, rows: Vec<Row>) -> RunInput {
        RunInput {
            info: RunInfo {
                run_id: id.to_owned(),
                repeat,
                ..RunInfo::default()
            },
            results: Vec::new(),
            judged: Some(Judged {
                judge: JudgeIdentity {
                    model: "-".to_owned(),
                    grader_version: "1".to_owned(),
                },
                rows,
            }),
        }
    }

    fn row(task: &str, repeat: u32, passed: u32) -> Row {
        Row::graded(
            task,
            repeat,
            crate::grader::Score {
                n_passed: passed,
                n_criteria: 2,
            },
        )
    }

    #[test]
    fn repeats_give_pass_at_k_and_task_spread() {
        let run = input(
            "r",
            2,
            vec![
                row("a", 1, 2),
                row("a", 2, 0),
                row("b", 1, 1),
                row("b", 2, 1),
                Row::ungraded("c", 1, "x"),
            ],
        );
        let report = build(&[run]);
        let stats = &report.runs[0];
        assert_eq!((stats.n, stats.n_ungraded), (4, 1));
        assert!((stats.score.unwrap() - 50.0).abs() < 1e-9);
        let repeats = stats.repeats.as_ref().unwrap();
        assert!((repeats.pass_at_1 - 25.0).abs() < 1e-9, "{repeats:?}");
        assert!((repeats.pass_at_n - 50.0).abs() < 1e-9, "{repeats:?}");
        assert!((repeats.task_sd["a"] - 50.0).abs() < 1e-9 && repeats.task_sd["b"].abs() < 1e-9);
        assert!(render(&report, false).contains("pass@2: 50.00%"));
    }

    #[test]
    fn a_comparison_within_noise_says_so() {
        let a = input("a", 1, vec![row("t1", 1, 2), row("t2", 1, 0)]);
        let b = input("b", 1, vec![row("t1", 1, 0), row("t2", 1, 2)]);
        let report = build(&[a, b]);
        let cmp = &report.comparisons[0];
        assert!(cmp.delta.abs() < 1e-9 && cmp.within_noise, "{cmp:?}");
        assert!(render(&report, false).contains("NOTE: difference within noise"));
    }

    #[test]
    fn a_constant_improvement_has_a_tight_interval() {
        let tight = bootstrap_ci(&[50.0, 50.0, 50.0]);
        assert!((tight[0] - 50.0).abs() < 1e-9 && (tight[1] - 50.0).abs() < 1e-9);
        let ci = bootstrap_ci(&[-10.0, 10.0, -5.0, 5.0]);
        assert!(ci[0] < 0.0 && ci[1] > 0.0, "{ci:?}");
    }
}

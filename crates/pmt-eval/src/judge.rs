//! Post-hoc judging (spec: replay-and-evaluation, Post-hoc judging;
//! Re-judging and judge identity): `prometheus eval judge` is the only
//! code path that reads grader material. Each executed, uncontaminated
//! task is scored by its [`Scorer`]; a failure or zero criteria is an
//! ungraded row (`frac = null`), never a zero. Rows merge into
//! `judged.json` by `(task_id, repeat)`, an existing graded row survives
//! an ungraded re-judge, and a changed judge model or grader version is
//! refused unless the rows go to a separate `judged.<namespace>.json`.

use std::fmt;
use std::path::{Path, PathBuf};

use futures_util::StreamExt as _;
use pmt_core::error::{Error, codes};
use serde::{Deserialize, Serialize};

use crate::grader::{BoxFuture, Score};
use crate::run::TaskResult;

/// Who judged a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JudgeIdentity {
    /// The judge model (`-` when no grader uses one).
    pub model: String,
    /// The grader version (the suite's graders' fingerprint, or the
    /// benchmark adapter's version).
    pub grader_version: String,
}

impl fmt::Display for JudgeIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "model {}, graders {}", self.model, self.grader_version)
    }
}

/// One `judged.json` row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Row {
    /// The task.
    pub task_id: String,
    /// The repeat.
    pub repeat: u32,
    /// Criteria met.
    pub n_passed: Option<u32>,
    /// Criteria in all.
    pub n_criteria: Option<u32>,
    /// Every criterion met.
    pub all_pass: Option<bool>,
    /// `n_passed / n_criteria`; `null` when ungraded.
    pub frac: Option<f64>,
    /// Whether the row has a score.
    pub graded: bool,
    /// Why it is ungraded.
    pub error: Option<String>,
}

impl Row {
    /// A graded row.
    #[must_use]
    pub fn graded(task_id: &str, repeat: u32, score: Score) -> Self {
        Self {
            task_id: task_id.to_owned(),
            repeat,
            n_passed: Some(score.n_passed),
            n_criteria: Some(score.n_criteria),
            all_pass: Some(score.n_passed == score.n_criteria),
            frac: Some(f64::from(score.n_passed) / f64::from(score.n_criteria)),
            graded: true,
            error: None,
        }
    }

    /// An ungraded row.
    #[must_use]
    pub fn ungraded(task_id: &str, repeat: u32, error: impl Into<String>) -> Self {
        Self {
            task_id: task_id.to_owned(),
            repeat,
            n_passed: None,
            n_criteria: None,
            all_pass: None,
            frac: None,
            graded: false,
            error: Some(error.into()),
        }
    }
}

/// `judged.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Judged {
    /// Who judged.
    pub judge: JudgeIdentity,
    /// One row per task and repeat.
    pub rows: Vec<Row>,
}

impl Judged {
    /// Every row has a score.
    #[must_use]
    pub fn complete(&self) -> bool {
        self.rows.iter().all(|r| r.graded)
    }
}

/// The judged file of `run_dir` for `namespace`.
#[must_use]
pub fn path(run_dir: &Path, namespace: Option<&str>) -> PathBuf {
    match namespace {
        Some(name) => run_dir.join(format!("judged.{name}.json")),
        None => run_dir.join("judged.json"),
    }
}

/// Merges `rows` judged by `judge` into `existing`.
///
/// # Errors
///
/// `eval.judge_changed` (1) when `existing` was judged by another model
/// or grader version.
pub fn merge(
    existing: Option<Judged>,
    judge: JudgeIdentity,
    rows: Vec<Row>,
) -> Result<Judged, Error> {
    let Some(mut merged) = existing else {
        return Ok(Judged { judge, rows });
    };
    if merged.judge != judge {
        return Err(Error::new(
            &codes::EVAL_JUDGE_CHANGED,
            format!(
                "judge changed for this run ({} → {judge}); use --judge-namespace to score separately",
                merged.judge
            ),
        ));
    }
    for row in rows {
        let at = merged
            .rows
            .iter()
            .position(|r| r.task_id == row.task_id && r.repeat == row.repeat);
        match at {
            // A flaky re-judge keeps the grade it would replace.
            Some(i) if merged.rows[i].graded && !row.graded => {}
            Some(i) => merged.rows[i] = row,
            None => merged.rows.push(row),
        }
    }
    merged
        .rows
        .sort_by(|a, b| (&a.task_id, a.repeat).cmp(&(&b.task_id, b.repeat)));
    Ok(merged)
}

/// Scores one executed task.
pub trait Scorer: Sync {
    /// The score of `result` (its unit directory is `unit`), or why it is
    /// ungraded.
    fn score<'a>(
        &'a self,
        result: &'a TaskResult,
        unit: &'a Path,
    ) -> BoxFuture<'a, Result<Score, String>>;
}

/// The rows of `results` (unit directories from `unit_of`), at most
/// `parallel` scored at once. Tasks that did not execute or are
/// contaminated are ungraded without asking the scorer.
pub async fn rows(
    results: &[TaskResult],
    unit_of: &(dyn Fn(&TaskResult) -> PathBuf + Sync),
    scorer: &dyn Scorer,
    parallel: usize,
) -> Vec<Row> {
    let jobs = results.iter().map(|result| async move {
        let (task, repeat) = (result.task_id.as_str(), result.repeat);
        if result.contaminated {
            return Row::ungraded(task, repeat, "contaminated: excluded from scoring");
        }
        if !result.executed() {
            let why = result
                .skipped
                .as_ref()
                .map(|s| format!("skipped: {s}"))
                .or_else(|| result.error.clone())
                .unwrap_or_else(|| "did not deliver".to_owned());
            return Row::ungraded(task, repeat, format!("not executed: {why}"));
        }
        match scorer.score(result, &unit_of(result)).await {
            Ok(score) => Row::graded(task, repeat, score),
            Err(why) => Row::ungraded(task, repeat, why),
        }
    });
    futures_util::stream::iter(jobs)
        .buffered(parallel.max(1))
        .collect()
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn judge(model: &str) -> JudgeIdentity {
        JudgeIdentity {
            model: model.to_owned(),
            grader_version: "1".to_owned(),
        }
    }

    fn score(n: u32) -> Score {
        Score {
            n_passed: n,
            n_criteria: 2,
        }
    }

    /// Scenario: Flaky re-judge keeps the grade.
    #[test]
    fn a_failed_re_judge_keeps_the_graded_row() {
        let first = merge(None, judge("m"), vec![Row::graded("t1", 1, score(2))]).unwrap();
        let again = merge(
            Some(first.clone()),
            judge("m"),
            vec![
                Row::ungraded("t1", 1, "judge timed out"),
                Row::graded("t2", 1, score(1)),
            ],
        )
        .unwrap();
        assert_eq!(again.rows[0], first.rows[0]);
        assert_eq!(again.rows[1].frac, Some(0.5));
        let changed = merge(Some(again), judge("other"), Vec::new()).unwrap_err();
        assert_eq!(changed.exit_code(), 1);
        assert_eq!(
            changed.message(),
            "judge changed for this run (model m, graders 1 → model other, graders 1); use --judge-namespace to score separately"
        );
    }
}

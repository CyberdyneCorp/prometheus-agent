//! Batch prompt datasets and their statistics (spec: replay-and-evaluation,
//! Batch prompt datasets; Batch statistics and quality filtering).
//!
//! A dataset is JSON lines, each `{"prompt", "id"?, "image"?,
//! "toolsets"?, "files"?}`; [`Dataset::load`] validates every line before
//! any task runs and names the first invalid one. [`Statistics`] sums what
//! the tasks did, zero-filling every tool of the enabled toolsets so the
//! schema is the same for every run, and [`filter_reason`] marks tasks a
//! training export should leave out.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use pmt_core::error::{Error, codes};
use serde::{Deserialize, Serialize};

/// One dataset line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Line {
    /// The task id: `id`, else `line-<n>`.
    pub id: String,
    /// The prompt.
    pub prompt: String,
    /// The container image of the task's terminal backend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// The task's toolsets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub toolsets: Option<Vec<String>>,
    /// Files (relative to the dataset) copied into the workspace.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
}

/// A validated dataset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dataset {
    /// The dataset file.
    pub path: PathBuf,
    /// Its lines (after `--max-samples`).
    pub lines: Vec<Line>,
}

#[derive(Deserialize)]
struct Raw {
    prompt: Option<serde_json::Value>,
    id: Option<serde_json::Value>,
    image: Option<String>,
    toolsets: Option<Vec<String>>,
    #[serde(default)]
    files: Vec<String>,
}

fn invalid(path: &Path, line: usize, problem: impl std::fmt::Display) -> Error {
    Error::new(
        &codes::EVAL_INVALID_DATASET,
        format!("{} line {line}: {problem}", path.display()),
    )
}

impl Dataset {
    /// Reads and validates `path`, keeping the first `max_samples` lines;
    /// toolsets must be among `known`.
    ///
    /// # Errors
    ///
    /// `eval.io` reading it; `eval.invalid_dataset` (65) naming the first
    /// invalid line.
    pub fn load(path: &Path, max_samples: Option<usize>, known: &[String]) -> Result<Self, Error> {
        let text =
            std::fs::read_to_string(path).map_err(|e| Error::io(&codes::EVAL_IO, path, &e))?;
        let base = path.parent().unwrap_or(Path::new("."));
        let mut lines: Vec<Line> = Vec::new();
        for (index, raw) in text.lines().enumerate() {
            if max_samples.is_some_and(|max| lines.len() >= max) {
                break;
            }
            if raw.trim().is_empty() {
                continue;
            }
            let n = index + 1;
            let line = parse_line(raw, n, base, known).map_err(|p| invalid(path, n, p))?;
            if lines.iter().any(|l| l.id == line.id) {
                return Err(invalid(path, n, format!("duplicate id '{}'", line.id)));
            }
            lines.push(line);
        }
        Ok(Self {
            path: path.to_path_buf(),
            lines,
        })
    }
}

fn parse_line(raw: &str, n: usize, base: &Path, known: &[String]) -> Result<Line, String> {
    let raw: Raw = serde_json::from_str(raw).map_err(|e| format!("not a JSON object: {e}"))?;
    let prompt = match raw.prompt {
        Some(serde_json::Value::String(p)) if !p.trim().is_empty() => p,
        Some(serde_json::Value::String(_)) => return Err("'prompt' is empty".to_owned()),
        Some(_) => return Err("'prompt' must be a string".to_owned()),
        None => return Err("missing key 'prompt'".to_owned()),
    };
    let id = match raw.id {
        None => format!("line-{n}"),
        Some(serde_json::Value::String(s)) if is_task_id(&s) => s,
        Some(serde_json::Value::Number(num)) => num.to_string(),
        Some(other) => return Err(format!("'id' {other} is not a usable task id")),
    };
    if let Some(unknown) = raw
        .toolsets
        .iter()
        .flatten()
        .find(|t| !known.iter().any(|k| k == *t))
    {
        return Err(format!("unknown toolset '{unknown}'"));
    }
    if let Some(missing) = raw
        .files
        .iter()
        .find(|f| !crate::suite::is_relative_inside(f) || !base.join(f).is_file())
    {
        return Err(format!("file '{missing}' is not beside the dataset"));
    }
    Ok(Line {
        id,
        prompt,
        image: raw.image,
        toolsets: raw.toolsets,
        files: raw.files,
    })
}

/// Ids name directories: letters, digits, `.`, `_` and `-`.
fn is_task_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && !id.starts_with('.')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// One model call of a task.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelUse {
    /// `provider/model`.
    pub model: String,
    /// Input tokens.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Its cost; `None` when unpriced.
    pub cost_usd: Option<f64>,
}

/// What one task did, read from its trajectory.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Trace {
    /// The task.
    pub task_id: String,
    /// It executed to a final answer.
    pub ok: bool,
    /// The budget skipped it.
    pub skipped: bool,
    /// Assistant turns.
    pub assistant_turns: u32,
    /// Assistant turns carrying reasoning.
    pub reasoning_turns: u32,
    /// Its final turn hit `--max-turns`.
    pub hit_max_turns: bool,
    /// Tool calls: name and whether it succeeded.
    pub tool_calls: Vec<(String, bool)>,
    /// Model calls.
    pub calls: Vec<ModelUse>,
}

/// Why a task is left out of exports, if it is.
#[must_use]
pub fn filter_reason(trace: &Trace) -> Option<&'static str> {
    if trace.skipped {
        return None;
    }
    if trace.assistant_turns == 0 {
        return Some("no assistant reply");
    }
    if trace.hit_max_turns {
        return Some("final turn hit --max-turns");
    }
    if !trace.tool_calls.is_empty() && trace.tool_calls.iter().all(|(_, ok)| !ok) {
        return Some("only failed tool calls");
    }
    None
}

/// Task counts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counts {
    /// Executed and kept.
    pub completed: usize,
    /// Did not execute.
    pub failed: usize,
    /// Stopped by the budget before starting.
    pub skipped: usize,
    /// Executed but filtered out.
    pub filtered: usize,
}

/// One model's usage.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelTotals {
    /// Calls.
    pub calls: u64,
    /// Input tokens.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Priced spend.
    pub cost_usd: f64,
    /// Calls without a price.
    pub unpriced_calls: u64,
}

/// One tool's counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCounts {
    /// Calls.
    pub count: u64,
    /// Successful calls.
    pub success: u64,
    /// Failed calls.
    pub failure: u64,
}

/// A filtered task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Filtered {
    /// The task.
    pub task_id: String,
    /// Why.
    pub reason: String,
}

/// `statistics.json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Statistics {
    /// Task counts.
    pub tasks: Counts,
    /// All input tokens.
    pub input_tokens: u64,
    /// All output tokens.
    pub output_tokens: u64,
    /// All priced spend.
    pub cost_usd: f64,
    /// Calls without a price.
    pub unpriced_calls: u64,
    /// Per `provider/model`.
    pub models: BTreeMap<String, ModelTotals>,
    /// Per tool, every tool of the enabled toolsets included.
    pub tools: BTreeMap<String, ToolCounts>,
    /// Fraction of assistant turns carrying reasoning.
    pub reasoning_fraction: f64,
    /// Tasks per number of assistant turns.
    pub turns_per_task: BTreeMap<u32, usize>,
    /// The filtered tasks and why.
    pub filtered: Vec<Filtered>,
}

impl Statistics {
    /// The statistics of `traces`, with `tools` (every tool of the
    /// enabled toolsets) zero-filled.
    #[must_use]
    pub fn of(traces: &[Trace], tools: &[String]) -> Self {
        let mut stats = Self {
            tools: tools
                .iter()
                .map(|t| (t.clone(), ToolCounts::default()))
                .collect(),
            ..Self::default()
        };
        let (mut turns, mut reasoning) = (0_u64, 0_u64);
        for trace in traces {
            stats.count(trace);
            turns += u64::from(trace.assistant_turns);
            reasoning += u64::from(trace.reasoning_turns);
        }
        #[allow(clippy::cast_precision_loss)]
        if turns > 0 {
            stats.reasoning_fraction = reasoning as f64 / turns as f64;
        }
        stats
    }

    fn count(&mut self, trace: &Trace) {
        match (trace.skipped, trace.ok, filter_reason(trace)) {
            (true, _, _) => self.tasks.skipped += 1,
            (false, false, _) => self.tasks.failed += 1,
            (false, true, Some(reason)) => {
                self.tasks.filtered += 1;
                self.filtered.push(Filtered {
                    task_id: trace.task_id.clone(),
                    reason: reason.to_owned(),
                });
            }
            (false, true, None) => self.tasks.completed += 1,
        }
        if !trace.skipped {
            *self
                .turns_per_task
                .entry(trace.assistant_turns)
                .or_default() += 1;
        }
        for (tool, ok) in &trace.tool_calls {
            let counts = self.tools.entry(tool.clone()).or_default();
            counts.count += 1;
            if *ok {
                counts.success += 1;
            } else {
                counts.failure += 1;
            }
        }
        for call in &trace.calls {
            self.input_tokens += call.input_tokens;
            self.output_tokens += call.output_tokens;
            let model = self.models.entry(call.model.clone()).or_default();
            model.calls += 1;
            model.input_tokens += call.input_tokens;
            model.output_tokens += call.output_tokens;
            if let Some(cost) = call.cost_usd {
                model.cost_usd += cost;
                self.cost_usd += cost;
            } else {
                model.unpriced_calls += 1;
                self.unpriced_calls += 1;
            }
        }
    }
}

/// The sessions of a batch run's filtered tasks (exports leave them out
/// unless `--include-filtered`).
#[must_use]
pub fn filtered_sessions(run: &crate::run::RunDir) -> std::collections::BTreeSet<String> {
    let Some(stats) = crate::run::read_json::<Statistics>(&run.dir.join("statistics.json")) else {
        return std::collections::BTreeSet::new();
    };
    let filtered: Vec<&str> = stats.filtered.iter().map(|f| f.task_id.as_str()).collect();
    run.results()
        .into_iter()
        .filter(|r| filtered.contains(&r.task_id.as_str()))
        .filter_map(|r| r.session_id)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scenario: Zero-filled tool stats.
    #[test]
    fn unused_tools_of_the_enabled_toolsets_are_zero_filled() {
        let trace = Trace {
            task_id: "a".to_owned(),
            ok: true,
            assistant_turns: 2,
            tool_calls: vec![("terminal".to_owned(), true)],
            ..Trace::default()
        };
        let stats = Statistics::of(&[trace], &["terminal".to_owned(), "web_search".to_owned()]);
        assert_eq!(stats.tools["web_search"], ToolCounts::default());
        assert_eq!(stats.tools["terminal"].success, 1);
        assert_eq!(stats.tasks.completed, 1);
    }

    #[test]
    fn filters_name_their_reason() {
        let base = Trace {
            ok: true,
            assistant_turns: 1,
            ..Trace::default()
        };
        assert_eq!(filter_reason(&base), None);
        let none = Trace {
            assistant_turns: 0,
            ..base.clone()
        };
        assert_eq!(filter_reason(&none), Some("no assistant reply"));
        let failing = Trace {
            tool_calls: vec![("web_search".to_owned(), false)],
            ..base.clone()
        };
        assert_eq!(filter_reason(&failing), Some("only failed tool calls"));
        let capped = Trace {
            hit_max_turns: true,
            ..base
        };
        assert_eq!(filter_reason(&capped), Some("final turn hit --max-turns"));
    }

    #[test]
    fn an_invalid_line_is_named_before_anything_runs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.jsonl");
        std::fs::write(&path, "{\"prompt\": \"a\"}\n{\"id\": \"x\"}\n").unwrap();
        let err = Dataset::load(&path, None, &[]).unwrap_err();
        assert_eq!(err.exit_code(), 65);
        assert!(
            err.message().ends_with("line 2: missing key 'prompt'"),
            "{err}"
        );
        let first = Dataset::load(&path, Some(1), &[]).unwrap();
        assert_eq!(first.lines[0].id, "line-1");
    }
}

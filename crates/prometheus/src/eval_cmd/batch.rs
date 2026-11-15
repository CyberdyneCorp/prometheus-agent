//! `prometheus eval batch` and `eval distributions` (spec:
//! replay-and-evaluation, Batch prompt datasets; Batch statistics and
//! quality filtering; Toolset distributions): every dataset line is an
//! ungraded task with the same isolated sessions, run layout, caching,
//! budget and resumption as `eval run`. The ephemeral system prompt rides
//! on every model call and is scrubbed from what the sessions keep; the
//! run's `statistics.json` sums what the tasks did and marks the tasks a
//! training export leaves out.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use clap::Args;
use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_eval::batch::{Dataset, Statistics, Trace};
use pmt_eval::boundary::Scanner;
use pmt_eval::distribution::{BUILTIN, Distribution};
use pmt_eval::run::{RunDir, TaskResult, write_json};
use pmt_store::Store;
use serde_json::{Value, json};

use super::exec::{self, Runner, Unit};
use super::run::{self as eval_run, known_toolsets};
use super::{CommonArgs, trace};
use crate::context::Context;

/// The default `--max-turns`.
pub const MAX_TURNS: u32 = 10;

/// `prometheus eval batch`.
#[derive(Debug, Clone, Args)]
pub struct BatchArgs {
    /// The JSON-lines dataset (`prompt`; optional `id`, `image`,
    /// `toolsets`, `files`).
    pub dataset: PathBuf,
    /// Model calls per task.
    #[arg(long, value_name = "N", default_value_t = MAX_TURNS)]
    pub max_turns: u32,
    /// Only the first N lines.
    #[arg(long, value_name = "N")]
    pub max_samples: Option<usize>,
    /// Reasoning effort of the task sessions.
    #[arg(long, value_name = "E")]
    pub reasoning_effort: Option<String>,
    /// The toolsets of every task (unless its line names its own).
    #[arg(
        long,
        value_name = "a,b",
        value_delimiter = ',',
        conflicts_with = "toolset_distribution"
    )]
    pub toolsets: Vec<String>,
    /// A system prompt sent with every model call and never stored
    /// (TEXT, or @FILE).
    #[arg(long, value_name = "TEXT|@FILE")]
    pub ephemeral_system_prompt: Option<String>,
    /// A JSON list of messages inserted as few-shot history before each
    /// prompt.
    #[arg(long, value_name = "FILE")]
    pub prefill: Option<PathBuf>,
    /// Shared flags.
    #[command(flatten)]
    pub common: CommonArgs,
}

fn invalid(message: String) -> Error {
    Error::new(&codes::EVAL_INVALID_DATASET, message)
}

/// The text of `--ephemeral-system-prompt` (`@FILE` reads the file).
fn ephemeral(arg: Option<&str>) -> Result<Option<String>, Error> {
    let Some(arg) = arg else { return Ok(None) };
    match arg.strip_prefix('@') {
        Some(file) => std::fs::read_to_string(file)
            .map(Some)
            .map_err(|e| invalid(format!("--ephemeral-system-prompt @{file}: {e}"))),
        None => Ok(Some(arg.to_owned())),
    }
}

/// The run-wide configuration fragments: the prefill file and reasoning.
fn fragments(args: &BatchArgs, scope: &ProfileScope) -> Result<Vec<String>, Error> {
    let mut out = Vec::new();
    if let Some(file) = &args.prefill {
        let path = file
            .canonicalize()
            .map_err(|e| invalid(format!("--prefill {}: {e}", file.display())))?;
        pmt_agent::prompt::prefill::read(scope.dir(), &path.to_string_lossy())
            .map_err(|e| invalid(format!("--prefill {}: {}", file.display(), e.message())))?;
        out.push(format!(
            "prompt.prefill_messages_file = {}",
            toml_string(&path.to_string_lossy())
        ));
    }
    if let Some(effort) = &args.reasoning_effort {
        out.push(format!("agent.reasoning_effort = {}", toml_string(effort)));
    }
    exec::merged(scope.config(), &out).map_err(|e| Error::new(&codes::CLI_USAGE, e))?;
    Ok(out)
}

fn toml_string(text: &str) -> String {
    toml::Value::String(text.to_owned()).to_string()
}

/// Every tool of `toolsets` (includes followed).
fn tools_of(toolsets: &BTreeSet<String>) -> Vec<String> {
    let all: BTreeMap<String, pmt_tools::toolset::Toolset> = pmt_tools::toolset::builtin()
        .into_iter()
        .map(|t| (t.name.clone(), t))
        .collect();
    let mut tools = BTreeSet::new();
    let mut todo: Vec<String> = toolsets.iter().cloned().collect();
    let mut seen = BTreeSet::new();
    while let Some(name) = todo.pop() {
        if !seen.insert(name.clone()) {
            continue;
        }
        if let Some(set) = all.get(&name) {
            tools.extend(set.tools.iter().cloned());
            todo.extend(set.includes.iter().cloned());
        }
    }
    tools.into_iter().collect()
}

fn units(dataset: &Dataset, args: &BatchArgs, dist: Option<&Distribution>) -> Vec<Unit> {
    let base = dataset.path.parent().unwrap_or(Path::new("."));
    dataset
        .lines
        .iter()
        .map(|line| Unit {
            task_id: line.id.clone(),
            repeat: 1,
            prompt: line.prompt.clone(),
            files: line
                .files
                .iter()
                .map(|f| (f.clone(), base.join(f)))
                .collect(),
            toolsets: line
                .toolsets
                .clone()
                .or_else(|| dist.map(|d| d.sample(args.common.seed, &line.id)))
                .unwrap_or_else(|| args.toolsets.clone()),
            timeout: Duration::from_secs(pmt_eval::suite::DEFAULT_TIMEOUT_SECS),
            image: line.image.clone(),
            ..Unit::default()
        })
        .collect()
}

/// Runs `prometheus eval batch`.
///
/// # Errors
///
/// `eval.invalid_dataset` (65) before any task runs; resumption, store
/// and I/O errors.
pub fn run(args: &BatchArgs, ctx: &Context<'_>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let known = known_toolsets();
    let dataset = Dataset::load(&args.dataset, args.max_samples, &known)?;
    if let Some(bad) = args.toolsets.iter().find(|t| !known.contains(t)) {
        return Err(invalid(format!("--toolsets: unknown toolset '{bad}'")));
    }
    let dist = args
        .common
        .toolset_distribution
        .as_deref()
        .map(|d| Distribution::resolve(d, &known))
        .transpose()?;
    let ephemeral = ephemeral(args.ephemeral_system_prompt.as_deref())?;
    let config = fragments(args, &scope)?;
    let run_id = super::run_id(args.common.run_id.as_deref())?;
    let mut info = eval_run::base_info(&scope, &args.common, None, &run_id)?;
    "batch".clone_into(&mut info.kind);
    info.suite = args
        .dataset
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let bytes =
        std::fs::read(&args.dataset).map_err(|e| Error::io(&codes::EVAL_IO, &args.dataset, &e))?;
    pmt_eval::run::digest(&bytes)[..12].clone_into(&mut info.suite_version);
    info.suite_path = args
        .dataset
        .canonicalize()
        .unwrap_or_else(|_| args.dataset.clone());
    info.budget_usd = args.common.budget_usd;
    let mut units = units(&dataset, args, dist.as_ref());
    for unit in &mut units {
        unit.config.clone_from(&config);
        info.toolsets
            .insert(unit.task_id.clone(), unit.toolsets.clone());
    }
    let run = RunDir::of(scope.dir(), &run_id);
    let info = eval_run::open_run(&run, info)?;
    let store = Arc::new(Store::open(&scope)?);
    let runner = Runner {
        scope: exec::isolated(
            &scope,
            args.common.cache.mode(),
            args.common.budget_usd,
            Some(args.max_turns),
        ),
        launch: ctx.launch.clone(),
        root: exec::root(&store, &run, &run_id)?,
        store,
        run: run.clone(),
        run_id: run_id.clone(),
        provider: args.common.provider.clone(),
        model: args.common.model.clone(),
        harness_version: info.harness_ref.clone(),
        ephemeral,
        scanner: Scanner::new(&[], &[]),
    };
    let quiet = ctx.json;
    let say = move |line: &str| {
        if !quiet {
            println!("{line}");
        }
    };
    let enabled = enabled_toolsets(&scope, &units);
    let outcome = crate::model_cmd::block_on(eval_run::execute(
        &runner,
        units,
        1,
        args.common.parallel,
        None,
        &say,
    ))??;
    statistics(&run, &enabled)?;
    eval_run::finish(&run, info, &outcome, ctx)
}

/// The toolsets the tasks ran with (the profile's for tasks naming none).
fn enabled_toolsets(scope: &ProfileScope, units: &[Unit]) -> BTreeSet<String> {
    let profile: Vec<String> = match &scope.config().tools.enabled_toolsets {
        list if list.is_empty() => vec!["core".to_owned()],
        list => list.clone(),
    };
    units
        .iter()
        .flat_map(|u| {
            if u.toolsets.is_empty() {
                profile.clone()
            } else {
                u.toolsets.clone()
            }
        })
        .collect()
}

/// Writes `statistics.json` over every task of the run (resumed ones
/// included).
///
/// # Errors
///
/// `eval.io`.
pub fn statistics(run: &RunDir, toolsets: &BTreeSet<String>) -> Result<Statistics, Error> {
    let traces: Vec<Trace> = run.results().iter().map(|r| trace_of(run, r)).collect();
    let stats = Statistics::of(&traces, &tools_of(toolsets));
    write_json(&run.dir.join("statistics.json"), &stats)?;
    Ok(stats)
}

fn trace_of(run: &RunDir, result: &TaskResult) -> Trace {
    let lines: Vec<Value> = std::fs::read_to_string(
        run.unit(&result.task_id, result.repeat)
            .join("trajectory.jsonl"),
    )
    .unwrap_or_default()
    .lines()
    .filter_map(|l| serde_json::from_str(l).ok())
    .collect();
    let capped = lines
        .iter()
        .rev()
        .find(|l| l["type"] == "turn.end")
        .is_some_and(|l| l["payload"]["exit_reason"] == "iteration_limit");
    let mut trace = trace::batch_trace(&result.task_id, result.ok, capped, &lines);
    trace.skipped = result.skipped.is_some();
    trace
}

/// Runs `prometheus eval distributions`.
///
/// # Errors
///
/// Output errors.
pub fn distributions(ctx: &Context<'_>) -> Result<u8, Error> {
    if ctx.json {
        let all: BTreeMap<&str, BTreeMap<&str, f64>> = BUILTIN
            .iter()
            .map(|(n, e)| (*n, e.iter().copied().collect()))
            .collect();
        return crate::output::json(&json!({"distributions": all}));
    }
    let mut text = String::new();
    for (name, entries) in BUILTIN {
        let list: Vec<String> = entries.iter().map(|(k, p)| format!("{k}={p}")).collect();
        let _ = writeln!(text, "{name}\t{}", list.join(" "));
    }
    crate::output::print_raw(&text)
}

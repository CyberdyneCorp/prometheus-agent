//! `prometheus eval run` (spec: replay-and-evaluation, Eval run; Eval runs
//! are resumable; Benchmark adapter protocol): every selected task and
//! repeat in its own session, at most `--parallel` at once, under one
//! `run.json`. A resumed run skips tasks whose completion marker still
//! matches their deliverables, re-runs the others after saying why, and
//! refuses (exit 2) a different suite version, model or harness. The run
//! exits 0 when every task executed, 1 when one failed to execute and 3
//! when the budget stopped it.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures_util::StreamExt as _;
use pmt_config::ProfileScope;
use pmt_config::harness::HarnessDir;
use pmt_core::error::Error;
use pmt_eval::bench::{Adapter, Describe, Phase};
use pmt_eval::boundary::Scanner;
use pmt_eval::distribution::Distribution;
use pmt_eval::run::{Resume, RunDir, RunInfo, TaskResult};
use pmt_eval::suite::{RunSpec, Suite};
use pmt_store::Store;
use serde_json::json;

use super::exec::{self, Hooks, Runner, Unit};
use super::{CommonArgs, RunArgs};
use crate::context::Context;

/// Where a run's tasks come from.
pub enum Source {
    /// A suite.
    Suite(Suite),
    /// A benchmark adapter and its task ids.
    Bench {
        /// The adapter (opened for execution: it cannot be asked to score).
        adapter: Mutex<Adapter>,
        /// What it described.
        describe: Describe,
        /// The split's tasks.
        tasks: Vec<String>,
    },
}

/// What a planned run is.
pub struct Plan {
    /// `run.json` as this invocation would write it.
    pub info: RunInfo,
    /// The units, in order.
    pub units: Vec<Unit>,
    /// Units skipped before running (a capability this platform lacks).
    pub skipped: Vec<(Unit, String)>,
    /// Attempts on grader material.
    pub scanner: Scanner,
}

/// The toolsets of every registered toolset (distributions and datasets
/// are checked against them).
#[must_use]
pub fn known_toolsets() -> Vec<String> {
    let mut names: Vec<String> = pmt_tools::toolset::builtin()
        .into_iter()
        .map(|t| t.name)
        .collect();
    names.sort();
    names.dedup();
    names
}

/// The provider and model a run names (for `run.json` and resumption).
#[must_use]
pub fn route_of(scope: &ProfileScope, common: &CommonArgs) -> (String, String) {
    let model = &scope.config().model;
    (
        common
            .provider
            .clone()
            .or_else(|| model.provider.clone())
            .unwrap_or_else(|| "auto".to_owned()),
        common
            .model
            .clone()
            .or_else(|| model.default.clone())
            .unwrap_or_else(|| "default".to_owned()),
    )
}

/// The harness version `reference` names (the active one by default) and
/// its fingerprint.
///
/// # Errors
///
/// `harness.version_not_found` (66), harness I/O.
pub fn harness_of(
    scope: &ProfileScope,
    reference: Option<&str>,
) -> Result<(String, String), Error> {
    let dir = HarnessDir::of(scope.dir());
    let id = match reference {
        Some(r) => dir.resolve(r)?.id,
        None => dir.active_id()?,
    };
    let hash = dir.tree(&id)?.fingerprint();
    Ok((id, hash))
}

/// A fresh `run.json` for `common`.
///
/// # Errors
///
/// As [`harness_of`].
pub fn base_info(
    scope: &ProfileScope,
    common: &CommonArgs,
    harness: Option<&str>,
    run_id: &str,
) -> Result<RunInfo, Error> {
    let (provider, model) = route_of(scope, common);
    let (harness_ref, harness_hash) = harness_of(scope, harness)?;
    Ok(RunInfo {
        run_id: run_id.to_owned(),
        provider,
        model,
        harness_ref,
        harness_hash,
        prometheus_build: format!(
            "prometheus {} ({})",
            crate::build_info::VERSION,
            crate::build_info::COMMIT
        ),
        cache: common.cache.as_str().to_owned(),
        repeat: 1,
        seed: common.seed,
        toolset_distribution: common.toolset_distribution.clone(),
        started_at: jiff::Timestamp::now().to_string(),
        ..RunInfo::default()
    })
}

impl Source {
    /// The suite, or the adapter `bench:<name>[:<split>]` names.
    ///
    /// # Errors
    ///
    /// Suite errors; `eval.bench_failed` when the adapter fails to describe
    /// itself, pass its preflight or list its tasks.
    pub fn open(scope: &ProfileScope, spec: &str) -> Result<Self, Error> {
        let Some((name, split)) = pmt_eval::bench::parse_ref(spec) else {
            return super::suites::load(scope, spec).map(Self::Suite);
        };
        let mut adapter = Adapter::open(scope.dir(), name, Phase::Execute)?;
        let describe = adapter.describe()?;
        adapter.preflight()?;
        let tasks = adapter.tasks(split)?;
        Ok(Self::Bench {
            adapter: Mutex::new(adapter),
            describe,
            tasks,
        })
    }

    fn plan(&self, args: &RunArgs, mut info: RunInfo) -> Result<Plan, Error> {
        let dist = args
            .common
            .toolset_distribution
            .as_deref()
            .map(|d| Distribution::resolve(d, &known_toolsets()))
            .transpose()?;
        let mock = info.provider == pmt_providers::mock::PROVIDER_ID;
        let (mut units, mut skipped, scanner) = match self {
            Self::Suite(suite) => suite_units(suite, args, mock, &mut info)?,
            Self::Bench {
                adapter,
                describe,
                tasks,
            } => bench_units(adapter, describe, tasks, args, &mut info)?,
        };
        if let Some(dist) = &dist {
            for unit in units.iter_mut().chain(skipped.iter_mut().map(|(u, _)| u)) {
                unit.toolsets = dist.sample(args.common.seed, &unit.task_id);
            }
        }
        for unit in &units {
            info.toolsets
                .insert(unit.task_id.clone(), unit.toolsets.clone());
        }
        info.repeat = args.repeat.max(1);
        Ok(Plan {
            info,
            units,
            skipped,
            scanner,
        })
    }
}

type Units = (Vec<Unit>, Vec<(Unit, String)>, Scanner);

fn suite_units(
    suite: &Suite,
    args: &RunArgs,
    mock: bool,
    info: &mut RunInfo,
) -> Result<Units, Error> {
    "suite".clone_into(&mut info.kind);
    suite.meta.name.clone_into(&mut info.suite);
    suite.meta.version.clone_into(&mut info.suite_version);
    info.suite_path.clone_from(&suite.dir);
    let (mut units, mut skipped) = (Vec::new(), Vec::new());
    for task in suite.select(&args.tasks)? {
        for repeat in 1..=args.repeat.max(1) {
            let unit = Unit {
                task_id: task.id.clone(),
                repeat,
                prompt: task.meta.prompt.clone(),
                then: task.run.then.clone(),
                files: task
                    .meta
                    .files
                    .iter()
                    .map(|f| (f.clone(), task.file(f)))
                    .collect(),
                toolsets: task
                    .meta
                    .toolsets
                    .clone()
                    .unwrap_or_else(|| suite.meta.default_toolsets.clone()),
                timeout: Duration::from_secs(
                    task.meta
                        .timeout_secs
                        .unwrap_or(suite.meta.default_timeout_secs),
                ),
                mock_script: task.mock_script.clone().filter(|_| mock),
                config: fragments(&task.run),
                interrupt: task.run.interrupt_after_turn_start,
                ..Unit::default()
            };
            match task.run.requires.as_deref().filter(|c| !available(c)) {
                Some(needed) => skipped.push((unit, format!("needs {needed}"))),
                None => units.push(unit),
            }
        }
    }
    let graders: Vec<_> = suite
        .tasks
        .iter()
        .map(pmt_eval::suite::Task::grader_dir)
        .collect();
    Ok((
        units,
        skipped,
        Scanner::new(&graders, &suite.meta.leak_globs),
    ))
}

/// The harness capabilities eval task sessions have: `interrupt` where
/// the platform can, never `memory` (task sessions are kept away from the
/// profile's memory).
fn available(capability: &str) -> bool {
    crate::smoke_suite::runner::interrupts(capability)
}

/// `run.toml`'s overrides as configuration fragments.
fn fragments(run: &RunSpec) -> Vec<String> {
    let mut out = run.set.clone();
    if !run.config.trim().is_empty() {
        out.push(run.config.clone());
    }
    out
}

fn bench_units(
    adapter: &Mutex<Adapter>,
    describe: &Describe,
    tasks: &[String],
    args: &RunArgs,
    info: &mut RunInfo,
) -> Result<Units, Error> {
    "bench".clone_into(&mut info.kind);
    info.suite = format!("bench:{}", describe.name);
    describe.version.clone_into(&mut info.suite_version);
    let mut adapter = adapter
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    info.suite_path = adapter.dir().to_path_buf();
    let mut units = Vec::new();
    for id in tasks
        .iter()
        .filter(|t| args.tasks.is_empty() || args.tasks.contains(t))
    {
        let task = adapter.task(id)?;
        for repeat in 1..=args.repeat.max(1) {
            units.push(Unit {
                task_id: id.clone(),
                repeat,
                prompt: task.prompt.clone(),
                texts: task
                    .files
                    .iter()
                    .map(|f| (f.path.clone(), f.content.clone()))
                    .collect(),
                timeout: Duration::from_secs(
                    task.timeout_secs
                        .unwrap_or(pmt_eval::suite::DEFAULT_TIMEOUT_SECS),
                ),
                ..Unit::default()
            });
        }
    }
    Ok((units, Vec::new(), Scanner::new(&[], &describe.leak_globs)))
}

/// A benchmark adapter around each task session.
struct BenchHooks<'a>(&'a Mutex<Adapter>);

impl Hooks for BenchHooks<'_> {
    fn prepare(&self, unit: &Unit, workspace: &std::path::Path) -> Result<(), String> {
        let mut adapter = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        adapter
            .prepare(&unit.task_id, workspace)
            .map_err(|e| e.message().to_owned())
    }

    fn collect(&self, unit: &Unit, unit_dir: &std::path::Path) -> Result<(), String> {
        let mut adapter = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        adapter
            .deliverables(&unit.task_id, unit_dir)
            .map_err(|e| e.message().to_owned())
    }
}

/// How a run ended.
#[derive(Debug, Default)]
pub struct Outcome {
    /// Every result written by this invocation (skipped ones included).
    pub results: Vec<TaskResult>,
    /// Units skipped as already complete.
    pub resumed: usize,
}

impl Outcome {
    /// 3 when the budget stopped the run, 1 when a task failed to execute,
    /// else 0.
    #[must_use]
    pub fn exit(&self) -> u8 {
        if self
            .results
            .iter()
            .any(|r| r.budget_stopped || r.skipped.as_deref() == Some("budget"))
        {
            pmt_core::exit::BUDGET_EXHAUSTED
        } else if self.results.iter().any(|r| r.skipped.is_none() && !r.ok) {
            pmt_core::exit::FAILURE
        } else {
            pmt_core::exit::SUCCESS
        }
    }
}

/// A skipped unit's result.
fn skipped(unit: &Unit, why: &str) -> TaskResult {
    TaskResult {
        task_id: unit.task_id.clone(),
        repeat: unit.repeat,
        skipped: Some(why.to_owned()),
        error: Some(format!("skipped: {why}")),
        toolsets: unit.toolsets.clone(),
        ..TaskResult::default()
    }
}

/// The label a unit is printed with.
#[must_use]
pub fn label(unit: &Unit, repeats: u32) -> String {
    if repeats > 1 {
        format!("{}#{}", unit.task_id, unit.repeat)
    } else {
        unit.task_id.clone()
    }
}

/// Runs `units` (resuming `runner.run`), printing progress with `say`.
///
/// # Errors
///
/// `eval.io` writing results.
pub async fn execute(
    runner: &Runner,
    units: Vec<Unit>,
    repeats: u32,
    parallel: usize,
    hooks: Option<&dyn Hooks>,
    say: &(dyn Fn(&str) + Sync),
) -> Result<Outcome, Error> {
    let stopped = AtomicBool::new(false);
    let mut todo = Vec::new();
    let mut outcome = Outcome::default();
    for unit in units {
        match runner.run.resume(&unit.task_id, unit.repeat) {
            Resume::Skip => outcome.resumed += 1,
            Resume::Rerun(why) => {
                say(&format!("re-running {}: {why}", label(&unit, repeats)));
                let _ = std::fs::remove_dir_all(runner.run.unit(&unit.task_id, unit.repeat));
                todo.push(unit);
            }
            Resume::Run => todo.push(unit),
        }
    }
    let jobs = todo.iter().map(|unit| {
        let stopped = &stopped;
        async move {
            if stopped.load(Ordering::SeqCst) {
                return skipped(unit, "budget");
            }
            let result = exec::execute(runner, unit, hooks).await;
            if result.budget_stopped {
                stopped.store(true, Ordering::SeqCst);
            }
            result
        }
    });
    let mut stream = futures_util::stream::iter(jobs).buffered(parallel.max(1));
    while let Some(result) = stream.next().await {
        runner.run.finish(&result)?;
        say(&progress(&result, repeats));
        outcome.results.push(result);
    }
    Ok(outcome)
}

fn progress(result: &TaskResult, repeats: u32) -> String {
    let name = if repeats > 1 {
        format!("{}#{}", result.task_id, result.repeat)
    } else {
        result.task_id.clone()
    };
    match (&result.skipped, result.ok, &result.error) {
        (Some(why), _, _) => format!("skipped: {name} ({why})"),
        (None, true, _) => format!("executed: {name} ({:.1} s)", result.wall_secs),
        (None, false, why) => format!(
            "FAILED: {name}: {}",
            why.as_deref().unwrap_or("did not execute")
        ),
    }
}

/// Writes `run.json` for `info`, refusing a resume that does not match.
///
/// # Errors
///
/// `eval.run_mismatch` (2); `eval.io`.
pub fn open_run(run: &RunDir, mut info: RunInfo) -> Result<RunInfo, Error> {
    if let Some(existing) = run.info() {
        existing.check_resume(&info)?;
        info.started_at = existing.started_at;
    }
    info.finished_at = None;
    run.write_info(&info)?;
    Ok(info)
}

/// Runs `prometheus eval run`.
///
/// # Errors
///
/// Suite, adapter, harness, resumption and store errors.
pub fn run(args: &RunArgs, ctx: &Context<'_>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let source = Source::open(&scope, &args.suite)?;
    let run_id = super::run_id(args.common.run_id.as_deref())?;
    let budget = args.common.budget_usd.or(match &source {
        Source::Suite(s) => s.meta.budget_usd,
        Source::Bench { .. } => None,
    });
    let mut info = base_info(&scope, &args.common, args.harness.as_deref(), &run_id)?;
    info.budget_usd = budget;
    let plan = source.plan(args, info)?;
    let run = RunDir::of(scope.dir(), &run_id);
    let info = open_run(&run, plan.info)?;
    let store = std::sync::Arc::new(Store::open(&scope)?);
    let runner = Runner {
        scope: exec::isolated(&scope, args.common.cache.mode(), budget, None),
        launch: ctx.launch.clone(),
        root: exec::root(&store, &run, &run_id)?,
        store,
        run: run.clone(),
        run_id: run_id.clone(),
        provider: args.common.provider.clone(),
        model: args.common.model.clone(),
        harness_version: info.harness_ref.clone(),
        ephemeral: None,
        scanner: plan.scanner,
    };
    let hooks = match &source {
        Source::Bench { adapter, .. } => Some(BenchHooks(adapter)),
        Source::Suite(_) => None,
    };
    let quiet = ctx.json;
    let say = move |line: &str| {
        if !quiet {
            println!("{line}");
        }
    };
    let mut outcome = crate::model_cmd::block_on(execute(
        &runner,
        plan.units,
        info.repeat,
        args.common.parallel,
        hooks.as_ref().map(|h| h as &dyn Hooks),
        &say,
    ))??;
    for (unit, why) in &plan.skipped {
        let result = skipped(unit, why);
        run.finish(&result)?;
        say(&progress(&result, info.repeat));
        outcome.results.push(result);
    }
    finish(&run, info, &outcome, ctx)
}

/// Stamps `run.json` and prints the summary; returns the exit status.
///
/// # Errors
///
/// `eval.io`.
pub fn finish(
    run: &RunDir,
    mut info: RunInfo,
    outcome: &Outcome,
    ctx: &Context<'_>,
) -> Result<u8, Error> {
    info.finished_at = Some(jiff::Timestamp::now().to_string());
    run.write_info(&info)?;
    let count = |f: &dyn Fn(&TaskResult) -> bool| outcome.results.iter().filter(|r| f(r)).count();
    let executed = count(&|r| r.skipped.is_none() && r.ok);
    let failed = count(&|r| r.skipped.is_none() && !r.ok);
    let skipped = count(&|r| r.skipped.is_some());
    let status = outcome.exit();
    if ctx.json {
        crate::output::document(
            "eval run",
            &json!({"run_id": info.run_id, "dir": run.dir, "executed": executed, "failed": failed, "skipped": skipped, "resumed": outcome.resumed, "exit": status}),
        )?;
    } else {
        println!(
            "run {}: {executed} executed, {failed} failed, {skipped} skipped, {} resumed ({})",
            info.run_id,
            outcome.resumed,
            run.dir.display()
        );
    }
    Ok(status)
}

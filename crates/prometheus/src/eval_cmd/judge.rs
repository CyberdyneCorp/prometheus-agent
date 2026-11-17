//! `prometheus eval judge` (spec: replay-and-evaluation, Post-hoc judging;
//! Re-judging and judge identity; Benchmark adapter protocol): the only
//! code path that reads grader material. Suite tasks are scored by their
//! grader — `script` graders run sandboxed (reading their `grader/` and
//! the deliverables, no network), `llm` graders ask the judge model on
//! the `eval_judge` auxiliary route, recorded with purpose `eval.judge` —
//! and benchmark tasks by the adapter's `bench.score`. Exit 0 when every
//! row is graded, 5 otherwise.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pmt_config::ProfileScope;
use pmt_core::env::LaunchEnv;
use pmt_core::error::{Error, codes};
use pmt_core::id::SessionId;
use pmt_eval::bench::{Adapter, Phase};
use pmt_eval::grader::{self, BoxFuture, Deliverables, Ports, Score};
use pmt_eval::judge::{self, JudgeIdentity, Scorer};
use pmt_eval::run::{RunDir, RunInfo, TaskResult, read_json, write_json};
use pmt_eval::suite::Suite;
use pmt_providers::auxiliary::{AuxArgs, AuxTask};
use pmt_store::Store;
use serde_json::json;

use super::JudgeArgs;
use crate::chat::session::model::{Models, RouteFlags};
use crate::context::Context;

/// How long a grader script may run.
const SCRIPT_TIMEOUT: Duration = Duration::from_mins(2);

/// The judge's script and model sides.
struct HostPorts {
    scope: ProfileScope,
    launch: LaunchEnv,
    models: Models,
    session: SessionId,
}

impl HostPorts {
    fn run_script(
        &self,
        command: &[String],
        grader_dir: &Path,
        deliverables: &Path,
    ) -> Result<String, String> {
        let scratch = deliverables
            .parent()
            .unwrap_or(deliverables)
            .join("grader-scratch");
        std::fs::create_dir_all(&scratch).map_err(|e| e.to_string())?;
        let paths = pmt_sandbox::Paths {
            user_home: self.launch.user_home().cloned().unwrap_or_default(),
            prometheus_home: self.scope.home().to_path_buf(),
            workspace: grader_dir.to_path_buf(),
            scratch: vec![scratch.clone()],
        };
        let deliverables_read = format!("fs.read:{}/**", deliverables.display());
        let caps = [
            "fs.read:<workspace>/**",
            "fs.read:<scratch>/**",
            "fs.write:<scratch>/**",
            "fs.read:<toolchain>",
            "process.spawn:*",
            deliverables_read.as_str(),
        ];
        let policy = pmt_sandbox::Policy::from_capabilities(&caps, &paths)
            .map_err(|e| e.message().to_owned())?;
        let sandbox = pmt_sandbox::Sandbox::new(policy, &self.scope.config().security.sandbox)
            .map_err(|e| e.message().to_owned())?;
        let (program, args) = command.split_first().ok_or("empty grader command")?;
        let program = if program.starts_with("./") {
            grader_dir.join(program).to_string_lossy().into_owned()
        } else {
            program.clone()
        };
        let mut cmd = sandbox
            .command(&program)
            .map_err(|e| e.message().to_owned())?;
        cmd.args(args)
            .arg(deliverables)
            .current_dir(grader_dir)
            .env("PMT_DELIVERABLES", deliverables)
            .env("TMPDIR", &scratch)
            .stdin(std::process::Stdio::null());
        let output = pmt_sandbox::container::run_bounded(cmd, SCRIPT_TIMEOUT)?;
        let _ = std::fs::remove_dir_all(&scratch);
        if !output.status.success() {
            return Err(format!(
                "grader script exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

impl Ports for HostPorts {
    fn script<'a>(
        &'a self,
        command: &'a [String],
        grader_dir: &'a Path,
        deliverables: &'a Path,
    ) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move { self.run_script(command, grader_dir, deliverables) })
    }

    fn llm<'a>(
        &'a self,
        model: Option<&'a str>,
        rubric: &'a str,
        deliverable: &'a str,
    ) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            let args = aux_args(model);
            let user = format!("# Rubric\n{rubric}\n\n# Deliverable\n{deliverable}");
            match self
                .models
                .ask_auxiliary_with(
                    AuxTask::EvalJudge,
                    &args,
                    self.session,
                    (grader::JUDGE_SYSTEM, &user),
                    400,
                )
                .await
            {
                Ok(Some(reply)) => Ok(reply),
                Ok(None) => Err("no judge model route".to_owned()),
                Err(err) => Err(format!("judge call failed: {}", err.message())),
            }
        })
    }
}

/// `provider/model` (or a bare model) as auxiliary route arguments.
#[must_use]
pub fn aux_args(model: Option<&str>) -> AuxArgs {
    match model.map(|m| m.split_once('/').map_or((None, m), |(p, m)| (Some(p), m))) {
        Some((provider, model)) => AuxArgs {
            provider: provider.map(str::to_owned),
            model: Some(model.to_owned()),
        },
        None => AuxArgs::default(),
    }
}

/// Scores suite tasks with their graders.
struct SuiteScorer<'a> {
    suite: &'a Suite,
    ports: &'a HostPorts,
}

impl Scorer for SuiteScorer<'_> {
    fn score<'a>(
        &'a self,
        result: &'a TaskResult,
        unit: &'a Path,
    ) -> BoxFuture<'a, Result<Score, String>> {
        Box::pin(async move {
            let task = self
                .suite
                .task(&result.task_id)
                .ok_or("the task is no longer in the suite")?;
            let spec = task.grader.as_ref().ok_or("the task has no grader")?;
            let dir = unit.join("deliverables");
            let deliverables = Deliverables {
                answer: std::fs::read_to_string(dir.join("answer.md")).unwrap_or_default(),
                exit_status: result.exit_status,
                dir,
            };
            grader::grade(spec, &task.grader_dir(), &deliverables, self.ports).await
        })
    }
}

/// Scores benchmark tasks with `bench.score`.
struct BenchScorer(Mutex<Adapter>);

impl Scorer for BenchScorer {
    fn score<'a>(
        &'a self,
        result: &'a TaskResult,
        unit: &'a Path,
    ) -> BoxFuture<'a, Result<Score, String>> {
        Box::pin(async move {
            let mut adapter = self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match adapter.score(&result.task_id, unit)? {
                Some(score) => Ok(score),
                None => Err("bench.score returned null".to_owned()),
            }
        })
    }
}

/// The grader version of a suite: its version and a fingerprint of every
/// `grader.toml`.
fn grader_version(suite: &Suite) -> String {
    let specs: Vec<_> = suite.tasks.iter().map(|t| (&t.id, &t.grader)).collect();
    let text = serde_json::to_string(&specs).unwrap_or_default();
    let digest = pmt_eval::run::digest(text.as_bytes());
    format!("{}:{}", suite.meta.version, &digest[..12])
}

/// The judge model of a suite: the `llm` graders' models, else `-`.
fn judge_model(suite: &Suite, scope: &ProfileScope) -> String {
    let mut models: Vec<String> = suite
        .tasks
        .iter()
        .filter_map(|t| t.grader.as_ref())
        .filter(|g| g.kind == grader::GraderKind::Llm)
        .map(|g| g.model.clone().unwrap_or_else(|| default_judge(scope)))
        .collect();
    models.sort();
    models.dedup();
    if models.is_empty() {
        "-".to_owned()
    } else {
        models.join(",")
    }
}

fn default_judge(scope: &ProfileScope) -> String {
    let config = scope.config();
    match config.auxiliary.eval_judge.as_ref() {
        Some(task) if task.provider.is_some() || task.model.is_some() => format!(
            "{}/{}",
            task.provider.as_deref().unwrap_or("auto"),
            task.model.as_deref().unwrap_or("default")
        ),
        _ => format!(
            "{}/{}",
            config.model.provider.as_deref().unwrap_or("auto"),
            config.model.default.as_deref().unwrap_or("default")
        ),
    }
}

/// The run's root session (judge calls are accounted to it).
fn root_of(run: &RunDir, store: &Store, info: &RunInfo) -> Result<SessionId, Error> {
    super::exec::root(store, run, &info.run_id)
}

/// Judges run `run_id` of `scope` (rows merged into `judged.json`, or the
/// namespace's file); the merged rows and their file.
///
/// # Errors
///
/// `eval.run_not_found` (66), `eval.judge_changed` (1), suite and adapter
/// errors, `eval.io`.
pub fn judge_run(
    scope: &ProfileScope,
    launch: &LaunchEnv,
    run_id: &str,
    namespace: Option<&str>,
    parallel: usize,
) -> Result<(judge::Judged, PathBuf), Error> {
    if let Some(name) = namespace {
        namespace_ok(name)?;
    }
    let run = RunDir::existing(scope.dir(), run_id)?;
    let info = run.info().unwrap_or_default();
    let results = run.results();
    let store = Arc::new(Store::open(scope)?);
    let ports = HostPorts {
        models: Models::open(
            scope,
            &store,
            launch.user_home().map(PathBuf::as_path),
            &RouteFlags::default(),
            None,
        )?,
        session: root_of(&run, &store, &info)?,
        scope: scope.clone(),
        launch: launch.clone(),
    };
    let unit_of = |r: &TaskResult| run.unit(&r.task_id, r.repeat);
    let (identity, rows) = if info.kind == "bench" {
        let name = info.suite.trim_start_matches("bench:");
        let mut adapter = super::bench::open(scope, name, Phase::Judge)?;
        let version = adapter.describe()?.version;
        let scorer = BenchScorer(Mutex::new(adapter));
        let identity = JudgeIdentity {
            model: "-".to_owned(),
            grader_version: format!("bench:{name}@{version}"),
        };
        (
            identity,
            crate::model_cmd::block_on(judge::rows(&results, &unit_of, &scorer, parallel))?,
        )
    } else {
        let suite = Suite::load(&info.suite_path)?;
        let identity = JudgeIdentity {
            model: judge_model(&suite, scope),
            grader_version: grader_version(&suite),
        };
        let scorer = SuiteScorer {
            suite: &suite,
            ports: &ports,
        };
        (
            identity,
            crate::model_cmd::block_on(judge::rows(&results, &unit_of, &scorer, parallel))?,
        )
    };
    let path = judge::path(&run.dir, namespace);
    let merged = judge::merge(read_json(&path), identity, rows)?;
    write_json(&path, &merged)?;
    Ok((merged, path))
}

/// Runs `prometheus eval judge`.
///
/// # Errors
///
/// As [`judge_run`].
pub fn run(args: &JudgeArgs, ctx: &Context<'_>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let (judged, path) = judge_run(
        &scope,
        ctx.launch,
        &args.run_id,
        args.judge_namespace.as_deref(),
        args.parallel,
    )?;
    report(&judged, &path, ctx)
}

fn report(judged: &judge::Judged, path: &Path, ctx: &Context<'_>) -> Result<u8, Error> {
    let graded = judged.rows.iter().filter(|r| r.graded).count();
    let status = if judged.complete() {
        pmt_core::exit::SUCCESS
    } else {
        pmt_core::exit::PARTIAL
    };
    if ctx.json {
        crate::output::document(
            "eval judge",
            &json!({"judged": path, "graded": graded, "ungraded": judged.rows.len() - graded, "rows": judged.rows}),
        )?;
        return Ok(status);
    }
    for row in judged.rows.iter().filter(|r| !r.graded) {
        println!(
            "ungraded: {}#{}: {}",
            row.task_id,
            row.repeat,
            row.error.as_deref().unwrap_or("?")
        );
    }
    println!(
        "judged {graded} of {} rows ({})",
        judged.rows.len(),
        path.display()
    );
    Ok(status)
}

/// A namespace names one file beside `judged.json`.
fn namespace_ok(name: &str) -> Result<(), Error> {
    if pmt_eval::suite::is_relative_inside(name) && !name.contains(['/', '\\']) {
        return Ok(());
    }
    Err(Error::new(
        &codes::CLI_USAGE,
        format!("invalid --judge-namespace '{name}'"),
    ))
}

//! One task's session (spec: replay-and-evaluation, Eval run; Answer-free
//! boundary): a fresh hidden `source = eval` child of the run's root
//! session, tagged with the run id, in a workspace of its own built from
//! the task's files only — grader material is never copied in, and the
//! session's sandbox reads nothing but its workspace. Memory is off and
//! approvals fail closed, so tasks neither read nor change the user's
//! state; the run's budget is the root lineage's `budgets.root_usd`.
//!
//! Afterwards the unit directory holds `trajectory.jsonl`,
//! `deliverables/` (`answer.md` and the workspace as `files/`) and
//! `result.json`, which records no score.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use pmt_agent::turn::{
    EventSink, ExitReason, Interrupt, Issuer, TurnInput, TurnOptions, TurnResult,
};
use pmt_config::ProfileScope;
use pmt_config::schema::{ApprovalMode, UnattendedApproval};
use pmt_core::cancel::CancelToken;
use pmt_core::env::LaunchEnv;
use pmt_core::error::{Error, codes};
use pmt_core::id::SessionId;
use pmt_eval::boundary::Scanner;
use pmt_eval::run::{RunDir, TaskResult};
use pmt_obs::trajectory::writer::Emit;
use pmt_store::{EndReason, LineageKind, NewSession, Source, Store, UsageScope};
use serde::Deserialize as _;

use super::trace;
use crate::chat::session::model::RouteFlags;
use crate::chat::session::{Factory, Options};

/// What every task of a run shares.
pub struct Runner {
    /// The profile, patched for the run (cache mode, run budget,
    /// isolation).
    pub scope: ProfileScope,
    /// The launch environment.
    pub launch: LaunchEnv,
    /// The profile's store.
    pub store: Arc<Store>,
    /// The run's hidden root session.
    pub root: SessionId,
    /// The run directory.
    pub run: RunDir,
    /// The run id (the tasks' source tag).
    pub run_id: String,
    /// `--provider`.
    pub provider: Option<String>,
    /// `--model`.
    pub model: Option<String>,
    /// The harness version the tasks run with.
    pub harness_version: String,
    /// `--ephemeral-system-prompt`: sent, never stored.
    pub ephemeral: Option<String>,
    /// Finds attempts on grader material.
    pub scanner: Scanner,
}

/// One task and repeat to run.
#[derive(Debug, Clone, Default)]
pub struct Unit {
    /// The task.
    pub task_id: String,
    /// The repeat (1-based).
    pub repeat: u32,
    /// The first prompt.
    pub prompt: String,
    /// Follow-up prompts on the same session.
    pub then: Vec<String>,
    /// Workspace files: path in the workspace and source.
    pub files: Vec<(String, PathBuf)>,
    /// Workspace files given as text.
    pub texts: Vec<(String, String)>,
    /// The task's toolsets (empty: the profile's).
    pub toolsets: Vec<String>,
    /// Its time limit.
    pub timeout: Duration,
    /// Its mock script (when the provider is `mock`).
    pub mock_script: Option<PathBuf>,
    /// Configuration fragments (TOML) merged over the run's.
    pub config: Vec<String>,
    /// Interrupt the first turn once it starts.
    pub interrupt: bool,
    /// The container image of its terminal backend.
    pub image: Option<String>,
}

/// Fills or reads a task's directories around its session (a benchmark
/// adapter's `bench.prepare` and `bench.deliverables`).
pub trait Hooks: Sync {
    /// Prepares `workspace` for `unit`.
    ///
    /// # Errors
    ///
    /// Why the task cannot run.
    fn prepare(&self, unit: &Unit, workspace: &Path) -> Result<(), String>;
    /// Lets the adapter collect `unit`'s deliverables into `unit_dir`.
    ///
    /// # Errors
    ///
    /// Why it could not.
    fn collect(&self, unit: &Unit, unit_dir: &Path) -> Result<(), String>;
}

/// The run-wide patch: isolated, unattended task sessions.
#[must_use]
pub fn isolated(
    scope: &ProfileScope,
    cache: pmt_config::schema::CacheMode,
    budget: Option<f64>,
    max_turns: Option<u32>,
) -> ProfileScope {
    scope.patched(move |config| {
        config.memory.nudge_interval = 0;
        config.memory.memory_enabled = false;
        config.memory.user_profile_enabled = false;
        config.approvals.mode = ApprovalMode::Manual;
        config.approvals.single_query_mode = UnattendedApproval::Deny;
        config.lsp.enabled = false;
        config.replay.cache.mode = cache;
        if let Some(usd) = budget {
            config.budgets.root_usd = usd;
        }
        if let Some(turns) = max_turns {
            config.agent.max_iterations = turns;
        }
    })
}

/// `config` with `fragments` (TOML documents, or `key=value` lines)
/// merged in.
///
/// # Errors
///
/// The first fragment that does not parse or names an unknown key.
pub fn merged(
    config: &pmt_config::Config,
    fragments: &[String],
) -> Result<pmt_config::Config, String> {
    let mut value = toml::Value::try_from(config).map_err(|e| e.to_string())?;
    for fragment in fragments {
        let table: toml::Table = fragment.parse().map_err(|e| format!("{fragment}: {e}"))?;
        merge(&mut value, toml::Value::Table(table));
    }
    pmt_config::Config::deserialize(value).map_err(|e| e.to_string())
}

fn merge(base: &mut toml::Value, over: toml::Value) {
    match (base, over) {
        (toml::Value::Table(base), toml::Value::Table(over)) => {
            for (key, value) in over {
                match base.get_mut(&key) {
                    Some(slot) => merge(slot, value),
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (slot, value) => *slot = value,
    }
}

/// Stops the first turn as soon as it starts (`interrupt_after_turn_start`).
#[derive(Default)]
struct InterruptOnStart {
    interrupt: Mutex<Option<Interrupt>>,
}

impl EventSink for InterruptOnStart {
    fn emit(&self, emit: Emit) -> pmt_agent::turn::model::BoxFuture<'_, Result<(), Error>> {
        if matches!(emit.event, pmt_obs::trajectory::events::Event::TurnStart(_))
            && let Some(interrupt) = self
                .interrupt
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
        {
            interrupt.stop(Issuer::User);
        }
        Box::pin(async { Ok(()) })
    }
}

fn io(path: &Path, e: &std::io::Error) -> Error {
    Error::io(&codes::EVAL_IO, path, e)
}

/// Builds the workspace of `unit` at `ws`.
fn workspace(unit: &Unit, ws: &Path) -> Result<(), Error> {
    std::fs::create_dir_all(ws).map_err(|e| io(ws, &e))?;
    let texts = unit.texts.iter().map(|(p, t)| (p, Some(t), None));
    let files = unit.files.iter().map(|(p, src)| (p, None, Some(src)));
    for (path, text, source) in texts.chain(files) {
        if !pmt_eval::suite::is_relative_inside(path) {
            return Err(Error::new(
                &codes::EVAL_INVALID_SUITE,
                format!("workspace path {path} leaves the workspace"),
            ));
        }
        let target = ws.join(path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| io(parent, &e))?;
        }
        match (text, source) {
            (Some(text), _) => std::fs::write(&target, text).map_err(|e| io(&target, &e))?,
            (None, Some(src)) => {
                std::fs::copy(src, &target).map_err(|e| io(src, &e))?;
            }
            (None, None) => {}
        }
    }
    Ok(())
}

/// Checks (or pulls) the container image of a task before any model call.
fn ensure_image(scope: &ProfileScope, image: &str) -> Result<(), String> {
    let unavailable = || format!("image not available: {image}");
    let runtime =
        pmt_sandbox::container::Runtime::detect(&scope.config().terminal.container.runtime)
            .map_err(|_| unavailable())?;
    let present = runtime
        .run(&["image".into(), "inspect".into(), image.to_owned()])
        .is_ok_and(|o| o.status.success());
    if present {
        return Ok(());
    }
    let pulled = runtime
        .run(&["pull".into(), image.to_owned()])
        .is_ok_and(|o| o.status.success());
    pulled.then_some(()).ok_or_else(unavailable)
}

/// The scope of `unit`: the run's, its toolsets, image and fragments.
fn unit_scope(runner: &Runner, unit: &Unit) -> Result<ProfileScope, String> {
    let config = merged(runner.scope.config(), &unit.config)?;
    let toolsets = unit.toolsets.clone();
    let image = unit.image.clone();
    Ok(runner.scope.patched(move |c| {
        *c = config;
        if !toolsets.is_empty() {
            c.tools.enabled_toolsets = toolsets;
        }
        if let Some(image) = image {
            "container".clone_into(&mut c.terminal.backend);
            c.terminal.container.image = image;
        }
    }))
}

/// What the session left.
struct Ran {
    id: SessionId,
    turn: Result<TurnResult, Error>,
}

async fn session(
    runner: &Runner,
    unit: &Unit,
    scope: ProfileScope,
    ws: &Path,
) -> Result<Ran, Error> {
    let flags = RouteFlags {
        provider: runner.provider.as_deref(),
        model: runner.model.as_deref(),
        mock_script: unit.mock_script.as_deref(),
    };
    let options = Options {
        interactive: false,
        cwd: ws.to_path_buf(),
        no_context_files: true,
        source_tag: Some(runner.run_id.clone()),
        ..Options::default()
    };
    let factory = Factory::with_store(
        scope,
        Arc::clone(&runner.store),
        &runner.launch,
        &flags,
        None,
        options,
    )?;
    let target = factory.models.primary(&flags).await?;
    let mut new = NewSession::child(Source::Eval, runner.root, LineageKind::Delegate);
    new.source_tag = Some(runner.run_id.clone());
    new.hidden = true;
    new.provider = Some(target.provider.clone());
    new.model.clone_from(&target.model);
    new.cwd = Some(ws.to_string_lossy().into_owned());
    let id = runner.store.create_session(&new)?.id;
    runner
        .store
        .set_session_harness(id, &runner.harness_version)?;
    let sink = Arc::new(InterruptOnStart::default());
    let mut clients = crate::evolve_host::session::detached();
    clients.events = Arc::clone(&sink) as Arc<dyn EventSink>;
    let live = factory.open_with(id, &target, Some(clients)).await?;
    live.agent.set_ephemeral_system(runner.ephemeral.clone());
    let cancel = CancelToken::new();
    let first = unit.interrupt.then_some(&*sink);
    let mut turn = one_turn(&live, &unit.prompt, unit.timeout, &cancel, first).await;
    for text in &unit.then {
        if !matches!(&turn, Ok(r) if r.exit_reason == ExitReason::Completed) {
            break;
        }
        turn = one_turn(&live, text, unit.timeout, &cancel, None).await;
    }
    let reason = match &turn {
        Ok(r) if r.exit_reason == ExitReason::Completed => EndReason::UserExit,
        _ => EndReason::Error,
    };
    live.close(&runner.store, reason).await;
    Ok(Ran { id, turn })
}

/// One turn of `live` within `timeout`; `interrupt` stops it once it
/// starts.
async fn one_turn(
    live: &crate::chat::session::Live,
    text: &str,
    timeout: Duration,
    cancel: &CancelToken,
    interrupt: Option<&InterruptOnStart>,
) -> Result<TurnResult, Error> {
    let mut options = TurnOptions::new(cancel);
    options.run_budget = Some(timeout);
    if let Some(sink) = interrupt {
        *sink
            .interrupt
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(options.interrupt.clone());
    }
    Box::pin(
        live.agent
            .run_turn(Some(&live.scope), live.id, TurnInput::text(text), options),
    )
    .await
}

/// The turn ran to an end the harness reached (any exit but a provider
/// failure, a crash or a lost lease).
fn executed(turn: &Result<TurnResult, Error>) -> Result<&TurnResult, String> {
    match turn {
        Ok(r)
            if matches!(
                r.exit_reason,
                ExitReason::ProviderFailed | ExitReason::Crashed | ExitReason::LeaseUnavailable
            ) =>
        {
            Err(r.error.as_ref().map_or_else(
                || r.exit_reason.as_str().to_owned(),
                |e| e.message().to_owned(),
            ))
        }
        Ok(r) => Ok(r),
        Err(err) => Err(err.message().to_owned()),
    }
}

/// The trajectory of `id` as JSON values, copied to `unit/trajectory.jsonl`.
fn keep_trajectory(scope: &ProfileScope, id: SessionId, unit: &Path) -> trace::Lines {
    let dir = scope.path(pmt_obs::trajectory::files::DIR);
    let lines = pmt_obs::trajectory::files::find(&dir, &id.to_string())
        .and_then(|path| pmt_obs::trajectory::read::read_file(&path).ok())
        .unwrap_or_default();
    let values: trace::Lines = lines
        .iter()
        .filter_map(|l| serde_json::to_value(l).ok())
        .collect();
    let text = values.iter().fold(String::new(), |mut out, v| {
        let _ = writeln!(out, "{v}");
        out
    });
    let _ = std::fs::write(unit.join("trajectory.jsonl"), text);
    values
}

/// Removes the ephemeral system prompt from the session's recording
/// journal, which keeps each canonical request.
fn scrub_recording(scope: &ProfileScope, id: SessionId, ephemeral: &str) {
    let path = pmt_obs::recording::path(scope.dir(), &id.to_string());
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    let escaped = serde_json::to_string(ephemeral).unwrap_or_default();
    let inner = escaped.trim_matches('"');
    if !inner.is_empty() && text.contains(inner) {
        let _ = std::fs::write(&path, text.replace(inner, ""));
    }
}

/// Runs `unit`; its outputs are in its unit directory afterwards.
pub async fn execute(runner: &Runner, unit: &Unit, hooks: Option<&dyn Hooks>) -> TaskResult {
    let started = Instant::now();
    let dir = runner.run.unit(&unit.task_id, unit.repeat);
    let mut result = TaskResult {
        task_id: unit.task_id.clone(),
        repeat: unit.repeat,
        toolsets: unit.toolsets.clone(),
        ..TaskResult::default()
    };
    if let Err(why) = run_in(runner, unit, hooks, &dir, &mut result).await {
        result.ok = false;
        result.error = Some(why);
    }
    result.wall_secs = (started.elapsed().as_secs_f64() * 100.0).round() / 100.0;
    result
}

async fn run_in(
    runner: &Runner,
    unit: &Unit,
    hooks: Option<&dyn Hooks>,
    dir: &Path,
    result: &mut TaskResult,
) -> Result<(), String> {
    let _ = std::fs::remove_dir_all(dir);
    let ws = dir.join("workspace");
    workspace(unit, &ws).map_err(|e| e.message().to_owned())?;
    // Sessions and their sandbox need the absolute path (a relative
    // `--home` makes the run directory relative).
    let ws = ws.canonicalize().map_err(|e| e.to_string())?;
    if let Some(hooks) = hooks {
        hooks.prepare(unit, &ws)?;
    }
    let scope = unit_scope(runner, unit)?;
    if let Some(image) = &unit.image {
        ensure_image(&scope, image)?;
    }
    let ran = Box::pin(session(runner, unit, scope.clone(), &ws))
        .await
        .map_err(|e| e.message().to_owned())?;
    result.session_id = Some(ran.id.to_string());
    if let Some(text) = &runner.ephemeral {
        scrub_recording(&scope, ran.id, text);
    }
    let lines = keep_trajectory(&scope, ran.id, dir);
    record_usage(&runner.store, ran.id, result);
    result.budget_stopped = trace::root_budget_refused(&lines);
    result.contamination = runner.scanner.attempts(&trace::tool_uses(&lines));
    result.contaminated = !result.contamination.is_empty();
    deliver(&ws, dir, &ran.turn)?;
    if let Some(hooks) = hooks {
        hooks.collect(unit, dir)?;
    }
    if result.budget_stopped {
        result.error = Some("the run's budget refused a model call".to_owned());
        result.exit_status = Some(pmt_core::exit::BUDGET_EXHAUSTED);
        return Ok(());
    }
    match executed(&ran.turn) {
        Ok(turn) => {
            result.ok = true;
            result.delivered = true;
            result.exit_status = Some(crate::chat::oneshot::status(turn));
            result.error = None;
        }
        Err(why) => {
            result.exit_status = ran.turn.as_ref().err().map(Error::exit_code);
            result.error = Some(why);
        }
    }
    Ok(())
}

/// `answer.md` and the workspace as `deliverables/files/`.
fn deliver(ws: &Path, dir: &Path, turn: &Result<TurnResult, Error>) -> Result<(), String> {
    let out = dir.join("deliverables");
    std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
    if let Some(answer) = turn.as_ref().ok().and_then(|r| r.final_response.as_deref()) {
        std::fs::write(out.join("answer.md"), answer).map_err(|e| e.to_string())?;
    }
    std::fs::rename(ws, out.join("files")).map_err(|e| e.to_string())
}

fn record_usage(store: &Store, id: SessionId, result: &mut TaskResult) {
    if let Ok(totals) = store.usage_totals(UsageScope::Session(id)) {
        result.cost_usd = totals.cost_usd;
        result.input_tokens = totals.tokens.input;
        result.output_tokens = totals.tokens.output;
        result.unpriced_calls = totals.unpriced_calls;
    }
}

/// The run's hidden root session (`<run>/root_session`), created once.
///
/// # Errors
///
/// Store errors.
pub fn root(store: &Store, run: &RunDir, run_id: &str) -> Result<SessionId, Error> {
    let file = run.dir.join("root_session");
    if let Some(id) = std::fs::read_to_string(&file)
        .ok()
        .and_then(|t| SessionId::parse(t.trim()).ok())
        .filter(|id| store.session(*id).is_ok())
    {
        return Ok(id);
    }
    let row = store.create_session(&NewSession {
        source_tag: Some(run_id.to_owned()),
        title: Some(format!("eval run {run_id}")),
        hidden: true,
        ..NewSession::new(Source::Eval)
    })?;
    std::fs::create_dir_all(&run.dir).map_err(|e| io(&run.dir, &e))?;
    std::fs::write(&file, row.id.to_string()).map_err(|e| io(&file, &e))?;
    Ok(row.id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fragments_merge_over_the_config() {
        let base = pmt_config::Config::default();
        let merged = merged(
            &base,
            &[
                "budgets.turn_tokens=600".to_owned(),
                "[models.\"mock/mock\"]\ncontext_window = 16000\n".to_owned(),
            ],
        )
        .unwrap();
        assert_eq!(merged.budgets.turn_tokens, 600);
        assert!(merged.models.contains_key("mock/mock"));
        assert!(super::merged(&base, &["nope.key=1".to_owned()]).is_err());
    }
}

//! Evolution jobs: the budget arithmetic ([`budget`]), task capture
//! ([`task`]), job records ([`record`]), the automatic trigger
//! ([`auto`]) and [`queue`], the one entry every trigger (CLI, `/evolve`,
//! scheduled job, kanban task, automatic) goes through.

pub mod auto;
pub mod budget;
pub mod record;
pub mod task;
pub mod workspace;

use std::path::{Path, PathBuf};

use pmt_config::Config;
use pmt_config::schema::EvolveMode;
use pmt_core::error::{Error, codes};
use pmt_core::id::SessionId;
use pmt_core::secret::Redactor;
use pmt_store::{Role, Store, Visibility};
use serde::Deserialize;
use serde_json::Value;

pub use record::{JobRecord, Jobs, PlanSummary, Trigger};

/// The domain of a job that names none.
pub const DEFAULT_DOMAIN: &str = "general";

/// Tools whose `path` argument names a workspace file the session read
/// or modified.
const FILE_TOOLS: &[&str] = &["read_file", "write_file", "patch"];

/// Where a job's task comes from.
#[derive(Debug, Clone)]
pub enum TaskInput {
    /// A recorded session: its first user message and the files it read
    /// or modified.
    Session(SessionId),
    /// A task file (`--task`, a scheduled job's `task`).
    File(PathBuf),
}

/// A request to queue a job.
#[derive(Debug, Clone)]
pub struct Request {
    /// What queues it.
    pub trigger: Trigger,
    /// The task.
    pub task: TaskInput,
    /// `--k` (default `evolve.k`).
    pub k: Option<u8>,
    /// `--mode` (default `evolve.mode`).
    pub mode: Option<EvolveMode>,
    /// `--domain` (default: the task file's, else `general`).
    pub domain: Option<String>,
    /// `--budget-usd`.
    pub budget_usd: Option<f64>,
    /// The scheduled job that fired it.
    pub cron_job_id: Option<String>,
    /// The kanban task it serves.
    pub kanban_task_id: Option<String>,
}

impl Request {
    /// A request with the profile's defaults.
    #[must_use]
    pub fn new(trigger: Trigger, task: TaskInput) -> Self {
        Self {
            trigger,
            task,
            k: None,
            mode: None,
            domain: None,
            budget_usd: None,
            cron_job_id: None,
            kanban_task_id: None,
        }
    }
}

/// The profile a job is queued in.
pub struct Profile<'a> {
    /// The profile directory.
    pub dir: &'a Path,
    /// Its name (messages).
    pub name: &'a str,
    /// Its config.
    pub config: &'a Config,
    /// Its store (session tasks).
    pub store: Option<&'a Store>,
    /// Its secrets, redacted from the task.
    pub redactor: Redactor,
}

/// Queues a job: checks the gate and the budget, freezes the task and
/// writes the record with status `queued`.
///
/// # Errors
///
/// `evolve.disabled` (78), `cli.usage` for k (2), `evolve.invalid_task`
/// (65), store and file errors.
pub fn queue(profile: &Profile<'_>, request: &Request) -> Result<JobRecord, Error> {
    queue_with(profile, request, |_| Ok(()))
}

/// [`queue`], with `before` run on the frozen `task/` directory before the
/// job is published (`evolve run --edit` opens `task.md` there); the task
/// fingerprint is computed after it.
///
/// # Errors
///
/// As [`queue`]; what `before` fails with.
pub fn queue_with(
    profile: &Profile<'_>,
    request: &Request,
    before: impl FnOnce(&Path) -> Result<(), Error>,
) -> Result<JobRecord, Error> {
    crate::gate::require_enabled(profile.config, profile.name)?;
    let evolve = &profile.config.evolve;
    let k = budget::check_k(request.k.unwrap_or(evolve.k))?;
    let mode = request.mode.unwrap_or(evolve.mode);
    let (capture, file_domain) = capture_of(profile, &request.task)?;
    let domain = request
        .domain
        .clone()
        .or(file_domain)
        .unwrap_or_else(|| DEFAULT_DOMAIN.to_owned());
    let record = JobRecord {
        id: Jobs::new_id(),
        status: "queued".to_owned(),
        trigger: request.trigger,
        cron_job_id: request.cron_job_id.clone(),
        kanban_task_id: request.kanban_task_id.clone(),
        source_session: capture.meta.source_session.clone(),
        mode: mode.as_str().to_owned(),
        k,
        domain: domain.clone(),
        budget_usd: request.budget_usd,
        created_at: jiff::Timestamp::now().to_string(),
        plan: budget::plans(mode, k)
            .iter()
            .map(|p| PlanSummary {
                mode: p.mode.to_owned(),
                rollouts: p.rollouts(),
                sessions: p.sessions(),
            })
            .collect(),
    };
    let mut capture = capture;
    capture.meta.domain = domain;
    Jobs::of(profile.dir).create(&record, |dir| {
        task::capture(dir, &capture)?;
        before(dir)?;
        task::refingerprint(dir)
    })?;
    tracing::info!(job = %record.id, trigger = record.trigger.as_str(), "evolution job queued");
    Ok(record)
}

fn invalid(message: impl Into<String>) -> Error {
    Error::new(&codes::EVOLVE_INVALID_TASK, message)
}

fn base_capture(profile: &Profile<'_>, statement: String) -> task::Capture {
    let config = profile.config;
    task::Capture {
        statement,
        workspace: None,
        files: None,
        meta: task::TaskMeta {
            backend: Some(config.terminal.backend.clone()),
            ..task::TaskMeta::default()
        },
        leak_globs: config.evolve.leak_globs.clone(),
        max_source_bytes: config.evolve.task.max_source_bytes,
        redactor: profile.redactor.clone(),
    }
}

/// The capture of `input` and the domain a task file names.
fn capture_of(
    profile: &Profile<'_>,
    input: &TaskInput,
) -> Result<(task::Capture, Option<String>), Error> {
    match input {
        TaskInput::Session(id) => {
            let store = profile
                .store
                .ok_or_else(|| invalid("a session task needs the profile's store"))?;
            session_capture(profile, store, *id).map(|c| (c, None))
        }
        TaskInput::File(path) => file_capture(profile, path),
    }
}

fn session_capture(
    profile: &Profile<'_>,
    store: &Store,
    id: SessionId,
) -> Result<task::Capture, Error> {
    let session = store.session(id)?;
    let messages = store.messages(id, Visibility::All)?;
    let statement = messages
        .iter()
        .find(|m| m.role == Role::User)
        .and_then(|m| m.content.clone())
        .filter(|c| !c.trim().is_empty())
        .ok_or_else(|| {
            invalid(format!(
                "session {id} has no user message to use as the task"
            ))
        })?;
    let files: Vec<PathBuf> = messages
        .iter()
        .filter_map(|m| m.tool_calls.as_ref())
        .flat_map(file_arguments)
        .map(PathBuf::from)
        .collect();
    let toolsets = store
        .tool_surface(id)?
        .map(|s| {
            let mut sets: Vec<String> = s
                .tools
                .into_iter()
                .filter(|t| t.exposure == "exposed")
                .map(|t| t.toolset)
                .collect();
            sets.sort();
            sets.dedup();
            sets
        })
        .unwrap_or_default();
    let mut capture = base_capture(profile, statement);
    capture.workspace = session.cwd.map(PathBuf::from);
    capture.files = Some(files);
    capture.meta.toolsets = toolsets;
    capture.meta.model.clone_from(&session.model);
    capture.meta.source_session = Some(id.to_string());
    Ok(capture)
}

/// The `path` arguments of file-tool calls in a `tool_calls` value.
fn file_arguments(calls: &Value) -> Vec<String> {
    calls
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c["name"].as_str().is_some_and(|n| FILE_TOOLS.contains(&n)))
        .filter_map(|c| {
            let arguments = match &c["arguments"] {
                Value::String(text) => serde_json::from_str::<Value>(text).ok()?,
                other => other.clone(),
            };
            arguments["path"].as_str().map(str::to_owned)
        })
        .collect()
}

/// A task file: TOML with `statement` (or `statement_file`), and optional
/// `domain`, `workspace` (relative to the file), `toolsets` and `model`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskFile {
    #[serde(default)]
    statement: Option<String>,
    #[serde(default)]
    statement_file: Option<PathBuf>,
    #[serde(default)]
    domain: Option<String>,
    #[serde(default)]
    workspace: Option<PathBuf>,
    #[serde(default)]
    toolsets: Vec<String>,
    #[serde(default)]
    model: Option<String>,
}

fn file_capture(
    profile: &Profile<'_>,
    path: &Path,
) -> Result<(task::Capture, Option<String>), Error> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| invalid(format!("task file {}: {e}", path.display())))?;
    let file: TaskFile = toml::from_str(&text)
        .map_err(|e| invalid(format!("task file {}: {}", path.display(), e.message())))?;
    let base = path.parent().unwrap_or_else(|| Path::new("."));
    let statement = match (file.statement, file.statement_file) {
        (Some(text), _) => text,
        (None, Some(rel)) => std::fs::read_to_string(base.join(&rel))
            .map_err(|e| invalid(format!("statement_file {}: {e}", rel.display())))?,
        (None, None) => {
            return Err(invalid(format!(
                "task file {} needs a statement",
                path.display()
            )));
        }
    };
    if statement.trim().is_empty() {
        return Err(invalid(format!(
            "task file {} has an empty statement",
            path.display()
        )));
    }
    let mut capture = base_capture(profile, statement);
    capture.workspace = file.workspace.map(|w| base.join(w));
    capture.meta.toolsets = file.toolsets;
    capture.meta.model = file.model;
    Ok((capture, file.domain))
}

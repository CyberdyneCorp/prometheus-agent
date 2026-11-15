//! The kanban side of the web dashboard (spec: web-dashboard, Kanban
//! dashboard API and orchestration modes): the shared boards, the
//! orchestration settings of the dispatcher's profile (the one the daemon
//! was started with, whose `[kanban]` the dispatcher applies) with their
//! configured and effective values, the active workers, one run, the
//! Terminate action and the dispatcher's inspect view. Everything here is
//! blocking (`SQLite`, process signals, `config.toml`): the daemon calls it
//! on a blocking thread.

use std::path::Path;

use pmt_config::{Launch, Overrides, ProfileName, ProfileScope};
use pmt_core::error::{Error, codes};
use pmt_kanban::model::Event;
use pmt_kanban::{Actor, Board, Boards, Filter, Run, Status, Task};
use serde_json::{Map, Value, json};

use super::assignees::Assignees;

/// The orchestration keys of `[kanban]` the settings panel edits.
const BOOL_KEYS: [&str; 2] = ["auto_decompose", "auto_promote_children"];
const PROFILE_KEYS: [&str; 2] = ["orchestrator_profile", "default_assignee"];

/// Events the inspect view shows.
const RECENT_EVENTS: usize = 20;

/// The statuses that wait for a worker (the backlog).
const BACKLOG: [Status; 6] = [
    Status::Triage,
    Status::Todo,
    Status::Ready,
    Status::Scheduled,
    Status::Blocked,
    Status::Review,
];

fn when(ms: Option<i64>) -> Value {
    ms.and_then(|ms| jiff::Timestamp::from_millisecond(ms).ok())
        .map_or(Value::Null, |t| t.to_string().into())
}

fn invalid(message: String) -> Error {
    Error::new(&codes::CONFIG_INVALID_ARGUMENT, message)
}

/// The profile `name` of the home at `home` as its `config.toml` says now
/// (not the daemon's last polled snapshot), so a save shows at once.
fn fresh(home: &Path, name: &str) -> Result<ProfileScope, Error> {
    let layout = pmt_config::layout::Home::open(home)?;
    let dir = layout.existing_profile(&ProfileName::parse(name)?)?;
    ProfileScope::open(dir, &Overrides::default(), Launch::Other)
}

fn task_row(task: &Task) -> Value {
    json!({
        "id": task.id,
        "title": task.title,
        "status": task.status.as_str(),
        "assignee": task.assignee,
        "priority": task.priority,
    })
}

/// `GET /api/kanban/boards`: every live board with its open tasks.
///
/// # Errors
///
/// `home.io`, store errors.
pub fn boards(home: &Path) -> Result<Value, Error> {
    let boards = Boards::open(home)?;
    let mut rows = Vec::new();
    for info in boards.list()?.into_iter().filter(|b| !b.settings.archived) {
        let board = boards.board(&info.slug)?;
        let tasks = board.tasks(&Filter::default())?;
        rows.push(json!({
            "slug": info.slug,
            "name": info.settings.name,
            "tasks": tasks.iter().map(task_row).collect::<Vec<_>>(),
        }));
    }
    Ok(json!({"boards": rows}))
}

/// The `[kanban]` orchestration keys as `config.toml` sets them (`null`
/// where it leaves the default).
fn configured(scope: &ProfileScope) -> Result<Map<String, Value>, Error> {
    let (text, _) = scope.profile().config_file().read()?;
    let table: toml::Table = toml::from_str(&text).unwrap_or_default();
    let kanban = table.get("kanban").and_then(toml::Value::as_table);
    Ok(BOOL_KEYS
        .iter()
        .chain(PROFILE_KEYS.iter())
        .map(|key| {
            let value = kanban
                .and_then(|k| k.get(*key))
                .and_then(|v| serde_json::to_value(v).ok())
                .unwrap_or(Value::Null);
            ((*key).to_owned(), value)
        })
        .collect())
}

/// The profiles of the home whose `kanban.role` is `orchestrator`.
fn orchestrators(scope: &ProfileScope) -> Vec<String> {
    super::orchestrate::assignee_names(scope)
        .into_iter()
        .filter(|name| super::orchestrate::is_orchestrator(scope.home(), name))
        .collect()
}

/// `GET /api/kanban/orchestration` for the dispatcher's profile
/// `profile`: what `config.toml` sets, what applies (defaults filled in,
/// a name that is no profile or lane of the home dropped and reported),
/// the orchestrator profiles and the possible assignees.
///
/// # Errors
///
/// The profile's configuration errors.
pub fn orchestration(home: &Path, profile: &str) -> Result<Value, Error> {
    let scope = fresh(home, profile)?;
    let kanban = &scope.config().kanban;
    let assignees = Assignees::of(&scope);
    let mut problems = Vec::new();
    let mut resolve = |key: &str, name: Option<&String>, lanes: bool| {
        let name = name?;
        let known = if lanes {
            pmt_kanban::board::create::assignee_lane(&assignees, name).is_ok()
        } else {
            super::profile_exists(home, name)
        };
        if !known {
            problems.push(format!(
                "kanban.{key} names '{name}', which is not a profile of this home"
            ));
        }
        known.then(|| name.clone())
    };
    let effective = json!({
        "mode": if kanban.auto_decompose { "auto" } else { "manual" },
        "auto_decompose": kanban.auto_decompose,
        "auto_promote_children": kanban.auto_promote_children,
        "orchestrator_profile": resolve("orchestrator_profile", kanban.orchestrator_profile.as_ref(), false),
        "default_assignee": resolve("default_assignee", kanban.default_assignee.as_ref(), true),
    });
    Ok(json!({
        "profile": profile,
        "configured": configured(&scope)?,
        "effective": effective,
        "problems": problems,
        "orchestrators": orchestrators(&scope),
        "assignees": super::orchestrate::assignee_names(&scope),
    }))
}

/// One requested change as `config.patch` takes it: a name, `true` or
/// `false`, or `None` to unset the key.
fn change_of(
    home: &Path,
    assignees: &Assignees,
    key: &str,
    value: &Value,
) -> Result<Option<String>, Error> {
    if BOOL_KEYS.contains(&key) {
        return value
            .as_bool()
            .map(|b| Some(b.to_string()))
            .ok_or_else(|| invalid(format!("kanban.{key} must be true or false")));
    }
    if !PROFILE_KEYS.contains(&key) {
        return Err(invalid(format!(
            "unknown orchestration setting '{key}' (expected {})",
            BOOL_KEYS
                .iter()
                .chain(PROFILE_KEYS.iter())
                .copied()
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    let name = match value {
        Value::Null => return Ok(None),
        Value::String(name) if name.trim().is_empty() => return Ok(None),
        Value::String(name) => name.trim(),
        _ => return Err(invalid(format!("kanban.{key} must be a profile name"))),
    };
    let known = if key == "default_assignee" {
        pmt_kanban::board::create::assignee_lane(assignees, name).is_ok()
    } else {
        super::profile_exists(home, name)
    };
    if known {
        Ok(Some(name.to_owned()))
    } else {
        Err(invalid(format!(
            "unknown profile '{name}' for kanban.{key}: `prometheus profile list` shows the profiles"
        )))
    }
}

/// `PUT /api/kanban/orchestration`: writes the keys `body` names to the
/// dispatcher profile's `config.toml` in one validated write (an empty or
/// `null` name unsets it); answers as [`orchestration`].
///
/// # Errors
///
/// `config.invalid_argument` (400) for an unknown key, a wrong type or a
/// name that is no profile; nothing is written then.
pub fn set_orchestration(home: &Path, profile: &str, body: &Value) -> Result<Value, Error> {
    let scope = fresh(home, profile)?;
    let assignees = Assignees::of(&scope);
    let Some(fields) = body.as_object() else {
        return Err(invalid("expected a JSON object of settings".to_owned()));
    };
    let mut changes = Vec::with_capacity(fields.len());
    for (key, value) in fields {
        let raw = change_of(home, &assignees, key, value)?;
        changes.push((format!("kanban.{key}"), raw));
    }
    if !changes.is_empty() {
        scope.profile().config_file().patch(&changes)?;
    }
    orchestration(home, profile)
}

fn worker_row(board: &Board, run: &Run) -> Value {
    let title = board
        .task(&run.task_id)
        .map(|t| t.title)
        .unwrap_or_default();
    json!({
        "worker_id": run.pid,
        "run_id": run.id,
        "board": board.slug(),
        "task_id": run.task_id,
        "task_title": title,
        "profile": run.profile,
        "role": run.role,
        "started_at": when(Some(run.started_at)),
        "last_heartbeat": when(run.heartbeat_at),
    })
}

/// `GET /api/kanban/workers/active`: every open run with a worker process,
/// oldest first.
///
/// # Errors
///
/// `home.io`, store errors.
pub fn active_workers(home: &Path) -> Result<Value, Error> {
    let boards = Boards::open(home)?;
    let mut rows = Vec::new();
    for slug in boards.live()? {
        let board = boards.board(&slug)?;
        for run in board.open_runs()?.iter().filter(|r| r.pid.is_some()) {
            rows.push((run.started_at, worker_row(&board, run)));
        }
    }
    rows.sort_by_key(|(started, _)| *started);
    let workers: Vec<Value> = rows.into_iter().map(|(_, row)| row).collect();
    Ok(json!({"count": workers.len(), "workers": workers}))
}

/// `GET /api/kanban/runs/{id}`.
///
/// # Errors
///
/// `kanban.run_not_found` (404), store errors.
pub fn run(home: &Path, id: &str) -> Result<Value, Error> {
    let board = Boards::open(home)?.board_of_run(id)?;
    let run = board.run(id)?;
    let log_name = format!("{}-{}.log", run.task_id, run.id);
    let log_exists = board.logs_dir().join(&log_name).is_file();
    Ok(json!({
        "run_id": run.id,
        "board": board.slug(),
        "task_id": run.task_id,
        "status": run.outcome.as_deref().unwrap_or("running"),
        "role": run.role,
        "profile": run.profile,
        "worker_id": run.pid,
        "started_at": when(Some(run.started_at)),
        "ended_at": when(run.ended_at),
        "exit_code": run.exit_code,
        "error": run.error,
        "log_name": log_name,
        "log_exists": log_exists,
    }))
}

/// `POST /api/kanban/runs/{id}/terminate` on behalf of dashboard user
/// `who`: stops the run's worker and frees its task for dispatch, with the
/// failure breaker of the dispatcher's profile `scope`.
///
/// # Errors
///
/// `kanban.run_not_found` (404), `kanban.illegal_transition` (409) when
/// the run already ended, store errors.
pub fn terminate(scope: &ProfileScope, id: &str, who: &str) -> Result<Value, Error> {
    let queue = super::hooks::HookQueue::new(scope);
    let boards = super::boards(scope, &queue)?;
    let mut board = boards.board_of_run(id)?;
    let breaker = super::breaker(&scope.config().kanban);
    let stopped = board.terminate(&Actor::user(who), id, breaker, pmt_kanban::now_ms());
    queue.flush(scope);
    let stopped = stopped?;
    Ok(json!({
        "ok": true,
        "run_id": id,
        "task_id": stopped.task_id,
        "status": stopped.status.as_str(),
        "signal": stopped.signal.map(pmt_kanban::process::Terminated::as_str),
    }))
}

fn event_row(board: &str, e: &Event) -> Value {
    json!({
        "board": board,
        "task_id": e.task_id,
        "run_id": e.run_id,
        "actor": e.actor,
        "kind": e.kind,
        "at": when(Some(e.at)),
        "at_ms": e.at,
    })
}

/// `GET /api/kanban/inspect` for the dispatcher's profile `scope`: the
/// backlog by status, running workers against `kanban.max_running`, and
/// the latest events of every live board, newest first.
///
/// # Errors
///
/// `home.io`, store errors.
pub fn inspect(scope: &ProfileScope) -> Result<Value, Error> {
    let boards = Boards::open(scope.home())?;
    let mut backlog: Map<String, Value> = BACKLOG
        .iter()
        .map(|s| (s.as_str().to_owned(), 0.into()))
        .collect();
    let mut running = 0_u64;
    let mut events = Vec::new();
    for slug in boards.live()? {
        let board = boards.board(&slug)?;
        for task in board.tasks(&Filter::default())? {
            if task.status == Status::Running {
                running += 1;
            } else if let Some(n) = backlog.get_mut(task.status.as_str()) {
                *n = (n.as_u64().unwrap_or(0) + 1).into();
            }
        }
        events.extend(
            board
                .recent_events(RECENT_EVENTS)?
                .iter()
                .map(|e| (e.at, e.id, event_row(&slug, e))),
        );
    }
    events.sort_by_key(|e| std::cmp::Reverse((e.0, e.1)));
    events.truncate(RECENT_EVENTS);
    let config = &scope.config().kanban;
    let settings = super::settings(config);
    Ok(json!({
        "backlog": backlog,
        "in_progress": running,
        "in_progress_limit": settings.max_running,
        "per_profile_limit": settings.max_running_per_profile,
        "dispatch": config.dispatch,
        "paused": pmt_core::pause::check(scope.home()).is_err(),
        "recent_events": events.into_iter().map(|(_, _, row)| row).collect::<Vec<_>>(),
    }))
}

/// `POST /api/kanban/boards/{board}/tasks/{task}/attachments`: stores the
/// upload under the dispatcher profile `scope`'s
/// `kanban.attachment_max_bytes` (re-checked here; nothing is stored when
/// it is too large).
///
/// # Errors
///
/// `kanban.attachment_too_large`, `kanban.board_not_found`,
/// `kanban.task_not_found`, `home.io`.
pub fn attach(
    scope: &ProfileScope,
    (slug, task): (&str, &str),
    who: &str,
    upload: &pmt_kanban::attach::Upload<'_>,
) -> Result<Value, Error> {
    let queue = super::hooks::HookQueue::new(scope);
    let boards = super::boards(scope, &queue)?;
    let mut board = boards.board(slug)?;
    let limit = scope.config().kanban.attachment_max_bytes;
    let stored = board.attach(&Actor::user(who), task, upload, limit, pmt_kanban::now_ms());
    queue.flush(scope);
    let a = stored?;
    Ok(json!({
        "id": a.id,
        "task_id": a.task_id,
        "filename": a.filename,
        "content_type": a.content_type,
        "size": a.size,
        "uploader": a.uploader,
        "path": board.attachment_path(&a),
    }))
}

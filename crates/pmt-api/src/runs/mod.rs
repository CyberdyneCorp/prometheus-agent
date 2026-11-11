//! The Runs API (spec: api-server, Runs API lifecycle; Run idempotency
//! ledger; Run events stream). `POST /v1/runs` answers 202 at once with a
//! `run_<ULID>`; the run's turn executes in the background, its status is
//! durable in the profile's store, and its events are buffered for
//! `GET /v1/runs/{id}/events`. Approvals and clarifications become pending
//! controls answered through `/approval` and `/clarify` (see
//! [`controls`]). A non-terminal run left by a daemon that crashed is
//! `unknown` until a client resolves it; one active at a clean shutdown
//! is `interrupted`.

mod controls;
mod events;
mod execute;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use pmt_store::Store;
use pmt_store::api::admissions::Endpoint;
use pmt_store::api::runs::{ApiRun, RunUpdate};
use serde_json::{Value, json};

use crate::admission::{self, Gate};
use crate::auth::Caller;
use crate::host::{ApiTurn, NewApiSession};
use crate::http::{
    Reply, Req, bad_request, from_error, json_reply, not_found, unavailable, with_header,
};
use crate::{Shared, model, responses, session};

pub(crate) use controls::control;
pub(crate) use events::Events;

/// The message of a run stopped by a daemon shutdown.
pub(crate) const SHUTDOWN: &str = "Gateway shutdown interrupted the run.";
/// How long a preview of tool arguments or results is.
const PREVIEW_CHARS: usize = 500;

/// A pending control of a run.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Pending {
    pub control: crate::turn::Control,
    pub generation: i64,
}

/// A run this daemon executes (or executed, while its buffer lives).
#[derive(Debug)]
pub(crate) struct Run {
    pub id: String,
    pub profile: String,
    pub session: String,
    /// The caller's session key, echoed on every event.
    pub session_key: Option<String>,
    pub store: Arc<Store>,
    pub events: Arc<Events>,
    pub state: Mutex<RunState>,
}

/// A run's live state.
#[derive(Debug, Default)]
pub(crate) struct RunState {
    pub generation: i64,
    pub pending: Vec<Pending>,
    pub stopping: bool,
    /// Its terminal status was recorded (by its turn or by a shutdown).
    pub done: bool,
}

impl Run {
    /// Records `update` in the store (unless the run already ended).
    pub(crate) fn record(&self, update: &RunUpdate) -> Option<ApiRun> {
        match self.store.update_api_run(&self.id, update) {
            Ok(run) => Some(run),
            Err(err) => {
                tracing::warn!(run = self.id, "run status not recorded: {err}");
                None
            }
        }
    }
}

/// The runs this daemon knows in memory.
#[derive(Debug, Default)]
pub(crate) struct Runs {
    live: Mutex<HashMap<String, Arc<Run>>>,
    /// Woken when an `unknown` run is resolved (its session's FIFO frees).
    pub(crate) resolved: tokio::sync::Notify,
}

impl Runs {
    pub(crate) fn get(&self, id: &str) -> Option<Arc<Run>> {
        self.live.lock().expect("runs poisoned").get(id).cloned()
    }

    fn insert(&self, run: Arc<Run>) {
        let mut live = self.live.lock().expect("runs poisoned");
        let now = std::time::Instant::now();
        live.retain(|_, run| !run.events.expire(now));
        live.insert(run.id.clone(), run);
    }

    fn all(&self) -> Vec<Arc<Run>> {
        self.live
            .lock()
            .expect("runs poisoned")
            .values()
            .cloned()
            .collect()
    }
}

/// `text` cut to [`PREVIEW_CHARS`] characters.
pub(crate) fn preview(text: &str) -> String {
    let mut out: String = text.chars().take(PREVIEW_CHARS).collect();
    if text.chars().count() > PREVIEW_CHARS {
        out.push('…');
    }
    out
}

/// Marks every run a previous daemon left unfinished `unknown`.
pub(crate) fn recover(shared: &Shared, profiles: &[String]) {
    let me = shared.server.instance_id().to_owned();
    for profile in profiles {
        let Some(store) = shared.host.api_store(profile) else {
            continue;
        };
        for run in store.active_api_runs().unwrap_or_default() {
            if run.owner != me && run.status != "unknown" {
                let update = RunUpdate {
                    status: Some("unknown".to_owned()),
                    ..RunUpdate::default()
                };
                if let Err(err) = store.update_api_run(&run.id, &update) {
                    tracing::warn!(run = run.id, "run not marked unknown: {err}");
                }
            }
        }
    }
}

/// Records every active run `interrupted` (the drain timed out).
pub(crate) fn interrupt_all(shared: &Shared) {
    for run in shared.runs.all() {
        {
            let mut state = run.state.lock().expect("run poisoned");
            if state.done {
                continue;
            }
            state.done = true;
            state.pending.clear();
        }
        let update = RunUpdate {
            status: Some("interrupted".to_owned()),
            error: Some(SHUTDOWN.to_owned()),
            ..RunUpdate::default()
        };
        run.record(&update);
        run.events.push(
            "run.interrupted",
            json!({"status": "interrupted", "error": SHUTDOWN}),
        );
    }
}

/// 503 while the daemon drains.
fn draining(shared: &Shared) -> Option<Reply> {
    shared.draining_since().map(|_| {
        unavailable(
            "runtime_draining",
            "the daemon is shutting down; retry after it restarts",
            10,
        )
    })
}

/// The identity fields of a run request.
const IDENTITY: [&str; 8] = [
    "input",
    "session_id",
    "instructions",
    "conversation_history",
    "previous_response_id",
    "model",
    "provider",
    "model_options",
];

/// `POST /v1/runs`.
pub(crate) async fn create(shared: &Arc<Shared>, caller: &Caller, req: &Req) -> Reply {
    match create_run(shared, caller, req).await {
        Ok(reply) | Err(reply) => reply,
    }
}

async fn create_run(shared: &Arc<Shared>, caller: &Caller, req: &Req) -> Result<Reply, Reply> {
    if let Some(refused) = draining(shared) {
        return Err(refused);
    }
    let body = req.json()?;
    let store = shared.store(&caller.profile)?;
    let config = shared.scope(&caller.profile)?.config().api_server.clone();
    let input = responses::input(&body["input"])?;
    let key = session::session_key(req)?;
    let target = existing_session(&store, &body, key.as_deref())?;
    let identity = admission::identity(&Value::Array(
        IDENTITY.iter().map(|f| body[*f].clone()).collect(),
    ));
    let idem = admission::key_of(req)?;
    let selected = model::select(shared, caller, &body, target.as_deref(), false).await?;
    let request = admission::Request {
        endpoint: Endpoint::Runs,
        key: idem.as_deref(),
        identity: &identity,
        session: target.as_deref(),
        conflict_code: "idempotency_key_conflict",
    };
    let finish =
        match admission::admit_run(shared, &store, caller, &request, config.max_concurrent_runs)
            .await?
        {
            Gate::Replay(run_id) => return Ok(replayed(&store, &run_id)),
            Gate::Run(finish) => finish,
        };
    let new = NewApiSession {
        instructions: body["instructions"].as_str().map(str::to_owned),
        session_key: key.clone(),
        derived_key: None,
        history: responses::history(&body["conversation_history"]).unwrap_or(input.earlier.clone()),
        title: None,
    };
    let refused = |err: &pmt_core::Error| {
        finish.release();
        from_error(err)
    };
    let (session_id, additions) = session::continue_or_create(shared, &caller.profile, target, new)
        .await
        .map_err(|e| refused(&e))?;
    let run = new_run(shared, &finish, &session_id, &selected);
    store.insert_api_run(&run).map_err(|e| refused(&e))?;
    if let Some(ticket) = &finish.ticket {
        admission::settle(&store, ticket, Some(&session_id), &run.id);
    }
    prune(&store);
    let turn = ApiTurn {
        session_id: session_id.clone(),
        text: input.last.text,
        images: input.last.images,
        additions,
        model: selected.choice,
        options: selected.options,
        unattended: None,
    };
    let live = Arc::new(Run {
        id: run.id.clone(),
        profile: caller.profile.clone(),
        session: session_id.clone(),
        session_key: key.clone(),
        store: Arc::clone(&store),
        events: Arc::new(Events::default()),
        state: Mutex::new(RunState {
            generation: 1,
            ..RunState::default()
        }),
    });
    shared.runs.insert(Arc::clone(&live));
    pmt_core::spawn::spawn(execute::execute(Arc::clone(shared), live, turn, finish));
    let reply = json_reply(
        202,
        &json!({"run_id": run.id, "status": "queued", "session_id": session_id}),
    );
    Ok(session::echo(reply, &session_id, key.as_deref()))
}

/// The record of a new run in `session`, queued.
fn new_run(
    shared: &Shared,
    finish: &admission::Finish,
    session: &str,
    selected: &model::Selected,
) -> ApiRun {
    let now = jiff::Timestamp::now();
    ApiRun {
        id: format!("run_{}", ulid::Ulid::generate()),
        session_id: session.to_owned(),
        status: "queued".to_owned(),
        admission_id: finish.ticket.as_ref().map_or_else(
            || format!("adm_{}", ulid::Ulid::generate()),
            |t| t.id.clone(),
        ),
        execution_generation: 1,
        owner: shared.server.instance_id().to_owned(),
        requested: Some(selected.route().to_string()),
        output: None,
        usage: None,
        runtime: None,
        error: None,
        created_at: now,
        updated_at: now,
    }
}

fn prune(store: &Store) {
    let now = jiff::Timestamp::now();
    if let Err(err) = store
        .prune_api_runs(now)
        .and_then(|_| store.prune_admissions(now))
    {
        tracing::debug!("run retention not applied: {err}");
    }
}

/// The 202 of an exact retry.
fn replayed(store: &Store, run_id: &str) -> Reply {
    let status = store
        .api_run(run_id)
        .ok()
        .flatten()
        .map_or_else(|| "unknown".to_owned(), |r| r.status);
    let reply = json_reply(202, &json!({"run_id": run_id, "status": status}));
    with_header(reply, "idempotency-replayed", "true")
}

/// The API session a run continues: `session_id` (owned by the API),
/// its previous response's, or the caller key's; `None` for a new one.
fn existing_session(
    store: &Arc<Store>,
    body: &Value,
    key: Option<&str>,
) -> Result<Option<String>, Reply> {
    if responses::history(&body["conversation_history"]).is_some() {
        return Ok(None);
    }
    if let Some(id) = body["session_id"].as_str() {
        return session::owned(store, id).map(|_| Some(id.to_owned()));
    }
    if let Some(previous) = body["previous_response_id"].as_str() {
        let found = store.api_response(previous).map_err(|e| from_error(&e))?;
        return found
            .map(|r| Some(r.session_id))
            .ok_or_else(|| not_found(&format!("response {previous}")));
    }
    match key {
        Some(key) => Ok(store
            .api_session_by_key(key)
            .map_err(|e| from_error(&e))?
            .map(|id| id.to_string())),
        None => Ok(None),
    }
}

/// `GET /v1/runs/{id}`.
pub(crate) fn get(shared: &Shared, caller: &Caller, id: &str) -> Reply {
    let store = match shared.store(&caller.profile) {
        Ok(store) => store,
        Err(reply) => return reply,
    };
    let run = match store.api_run(id) {
        Ok(Some(run)) => run,
        Ok(None) => return not_found(&format!("run {id}")),
        Err(err) => return from_error(&err),
    };
    let pending = shared.runs.get(id).map(|live| {
        live.state
            .lock()
            .expect("run poisoned")
            .pending
            .iter()
            .map(|p| {
                json!({"request_id": p.control.id, "kind": p.control.kind,
                "execution_generation": p.generation, "request": p.control.params})
            })
            .collect::<Vec<_>>()
    });
    let mut body = describe(&run);
    body["pending_controls"] = json!(pending.unwrap_or_default());
    if let Some(since) = shared.draining_since() {
        body["shutdown_requested_at"] = since.to_string().into();
    }
    json_reply(200, &body)
}

fn parsed(text: Option<&String>) -> Value {
    text.and_then(|t| serde_json::from_str(t).ok())
        .unwrap_or(Value::Null)
}

/// A run as the API shows it.
fn describe(run: &ApiRun) -> Value {
    json!({
        "object": "run",
        "run_id": run.id,
        "status": run.status,
        "session_id": run.session_id,
        "output": run.output,
        "error": run.error,
        "usage": parsed(run.usage.as_ref()),
        "runtime": parsed(run.runtime.as_ref()),
        "admission_id": run.admission_id,
        "execution_generation": run.execution_generation,
        "created_at": run.created_at.to_string(),
        "updated_at": run.updated_at.to_string(),
    })
}

/// `GET /v1/runs/{id}/events`.
pub(crate) fn events(shared: &Shared, caller: &Caller, id: &str, req: &Req) -> Reply {
    let store = match shared.store(&caller.profile) {
        Ok(store) => store,
        Err(reply) => return reply,
    };
    let run = match store.api_run(id) {
        Ok(Some(run)) => run,
        Ok(None) => return not_found(&format!("run {id}")),
        Err(err) => return from_error(&err),
    };
    let last = match req.header("last-event-id").map(str::parse::<u64>) {
        None => 0,
        Some(Ok(last)) => last,
        Some(Err(_)) => {
            return bad_request("invalid_last_event_id", "Last-Event-ID must be an event id");
        }
    };
    let (out, reply) = crate::http::sse_reply();
    if let Some(live) = shared
        .runs
        .get(id)
        .filter(|live| live.profile == caller.profile)
    {
        pmt_core::spawn::spawn(events::stream(
            Arc::clone(&live.events),
            (run.id, live.session_key.clone()),
            last,
            out,
        ));
    } else {
        let (event, mut data) = events::terminal_of(&run);
        data["event"] = event.clone().into();
        data["run_id"] = run.id.into();
        let _ = out.send(crate::http::Sse {
            id: None,
            event: Some(event),
            data: data.to_string(),
        });
    }
    reply
}

//! The additional verbs (spec: kanban, Additional CLI verbs; Per-task
//! model override and pinned skills; Worker lanes): `init`, `claim`,
//! `heartbeat`, `reassign`, `set-model`, `watch` and `assignees`, plus
//! the orchestration verbs of [`super::orchestrate`]. Each goes through
//! the board's typed operations, so the workflow table checks it and the
//! event log records it.

use std::fmt::Write as _;
use std::time::Duration;

use pmt_core::error::{Error, codes};
use pmt_kanban::ticket::{self, Ticket};
use pmt_kanban::{ClaimOutcome, Role};
use serde_json::json;

use super::{Env, KanbanCommand};

/// Runs one of these verbs; `None` when `command` is another's.
pub(super) fn run(env: &Env<'_>, command: &KanbanCommand) -> Option<Result<u8, Error>> {
    Some(match command {
        KanbanCommand::Init => init(env),
        KanbanCommand::Claim { id, ttl } => claim(env, id, *ttl),
        KanbanCommand::Heartbeat { id, note } => heartbeat(env, id, note.as_deref()),
        KanbanCommand::Reassign { args } => reassign(env, args),
        KanbanCommand::SetModel {
            id,
            model,
            provider,
        } => set_model(env, id, model, provider.as_deref()),
        KanbanCommand::Watch { assignee, kinds } => watch(env, assignee.as_deref(), kinds),
        KanbanCommand::Assignees => assignees(env),
        other => return super::orchestrate::run(env, other),
    })
}

/// `kanban init`: the default board exists (opening the boards made it)
/// and who dispatches.
fn init(env: &Env<'_>) -> Result<u8, Error> {
    let board = env.boards.board(pmt_kanban::DEFAULT_BOARD)?;
    let lease = board.lease()?;
    let now = pmt_kanban::now_ms();
    let dispatcher = match &lease {
        Some((holder, expires)) if *expires > now => format!("dispatcher: {holder}"),
        _ if env.scope.config().kanban.dispatch => {
            "dispatcher: none running (start `prometheus daemon start`, or run `prometheus kanban dispatch --loop`)".to_owned()
        }
        _ => "dispatcher: none (kanban.dispatch = false; run `prometheus kanban dispatch --loop`)".to_owned(),
    };
    if env.ctx.json {
        let holder = lease.filter(|(_, e)| *e > now).map(|(h, _)| h);
        return env.document(
            "kanban init",
            &json!({"board": board.slug(), "dir": board.dir(), "dispatcher": holder}),
        );
    }
    env.print(&format!(
        "board {} at {}\n{dispatcher}",
        board.slug(),
        board.dir().display()
    ))
}

/// `kanban claim`: a manual claim for an external worker.
fn claim(env: &Env<'_>, id: &str, ttl: Option<u64>) -> Result<u8, Error> {
    let mut board = env.board()?;
    let task = board.task(id)?;
    let profile = task
        .assignee
        .clone()
        .unwrap_or_else(|| env.scope.name().to_string());
    let ttl_ms = ttl.map_or_else(
        || crate::kanban_host::ttl_ms(&env.scope.config().kanban),
        crate::kanban_host::secs_ms,
    );
    let now = pmt_kanban::now_ms();
    let outcome = board.claim(&env.actor(), id, &profile, Role::Worker, ttl_ms, now)?;
    let ClaimOutcome::Claimed(claim) = outcome else {
        return Err(Error::new(
            &codes::KANBAN_ILLEGAL_TRANSITION,
            format!("{id} has open parents: it went back to todo"),
        ));
    };
    let line = Ticket {
        v: ticket::VERSION,
        board: board.slug().to_owned(),
        run: claim.run_id.clone(),
        secret: claim.secret.clone(),
    }
    .line();
    if env.ctx.json {
        return env.document(
            "kanban claim",
            &json!({"task_id": id, "run_id": claim.run_id, "profile": profile, "ticket": line}),
        );
    }
    env.print(&format!(
        "claimed {id} as {profile}: run {}\nticket (pass with --ticket; shown once): {line}",
        claim.run_id
    ))
}

/// `kanban heartbeat`: renews the open run's lease.
fn heartbeat(env: &Env<'_>, id: &str, note: Option<&str>) -> Result<u8, Error> {
    let mut board = env.board()?;
    let run = board
        .runs(id)?
        .into_iter()
        .rfind(pmt_kanban::Run::is_open)
        .ok_or_else(|| {
            Error::new(
                &codes::KANBAN_NOT_PERMITTED,
                format!("{id} has no open run"),
            )
        })?;
    let ttl = crate::kanban_host::ttl_ms(&env.scope.config().kanban);
    board.heartbeat(&env.actor(), &run.id, note, ttl, pmt_kanban::now_ms())?;
    env.print(&format!("renewed run {} of {id}", run.id))
}

/// `kanban reassign <id>... <profile>`.
fn reassign(env: &Env<'_>, args: &[String]) -> Result<u8, Error> {
    let Some((profile, ids)) = args.split_last() else {
        return Err(Error::new(
            &codes::CLI_USAGE,
            "reassign needs ids and a profile",
        ));
    };
    let profiles = env.profiles();
    pmt_kanban::board::create::assignee_lane(&profiles, profile)?;
    let mut board = env.board()?;
    let actor = env.actor();
    let mut out = String::new();
    let mut failed = false;
    for id in ids {
        match board.assign(&actor, id, Some(profile), &profiles, pmt_kanban::now_ms()) {
            Ok(()) => {
                let _ = writeln!(out, "ok {id} {profile}");
            }
            Err(err) => {
                failed = true;
                let _ = writeln!(out, "failed {id}: {}", err.message());
            }
        }
    }
    env.raw(&out)?;
    Ok(u8::from(failed))
}

/// `kanban set-model <id> <model>|none [--provider P]`.
fn set_model(env: &Env<'_>, id: &str, model: &str, provider: Option<&str>) -> Result<u8, Error> {
    let model = (model != "none").then_some(model);
    let mut board = env.board()?;
    board.set_model(
        &env.actor(),
        id,
        model,
        provider,
        &env.profiles(),
        pmt_kanban::now_ms(),
    )?;
    match model {
        Some(m) => env.print(&format!("{id} runs on {m} from its next dispatch")),
        None => env.print(&format!(
            "{id} runs on its assignee's model from its next dispatch"
        )),
    }
}

/// `kanban watch`: new board events as they happen (until interrupted).
fn watch(env: &Env<'_>, assignee: Option<&str>, kinds: &[String]) -> Result<u8, Error> {
    let board = env.board()?;
    let mut after = board.events(None)?.last().map_or(0, |e| e.id);
    let kinds: Vec<&str> = kinds.iter().map(String::as_str).collect();
    loop {
        let mut out = String::new();
        for event in board.events_since(after, &kinds)? {
            after = event.id;
            let owner = event
                .task_id
                .as_deref()
                .and_then(|id| board.task(id).ok())
                .and_then(|t| t.assignee);
            if assignee.is_some_and(|a| owner.as_deref() != Some(a)) {
                continue;
            }
            let _ = writeln!(
                out,
                "{}  {}  {}  {}",
                event.id,
                event.task_id.as_deref().unwrap_or("-"),
                event.kind,
                event.payload
            );
        }
        if !out.is_empty() {
            env.raw(&out)?;
        }
        if env.capture.is_some() {
            return Ok(0);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// `kanban assignees`: profiles and worker lanes.
fn assignees(env: &Env<'_>) -> Result<u8, Error> {
    let lanes = crate::kanban_host::lanes::declared(&env.scope);
    let profiles: Vec<String> = crate::kanban_host::orchestrate::assignee_names(&env.scope)
        .into_iter()
        .filter(|n| !lanes.iter().any(|l| &l.lane.name == n))
        .collect();
    if env.ctx.json {
        let lanes: Vec<_> = lanes
            .iter()
            .map(|l| json!({"name": l.lane.name, "plugin": l.plugin, "description": l.lane.description}))
            .collect();
        return env.document(
            "kanban assignees",
            &json!({"profiles": profiles, "lanes": lanes}),
        );
    }
    let mut out = String::new();
    for p in &profiles {
        let role = if crate::kanban_host::orchestrate::is_orchestrator(&env.home, p) {
            "  (orchestrator)"
        } else {
            ""
        };
        let _ = writeln!(out, "profile  {p}{role}");
    }
    for l in &lanes {
        let _ = writeln!(
            out,
            "lane     {}  ({})  {}",
            l.lane.name, l.plugin, l.lane.description
        );
    }
    env.raw(&out)
}

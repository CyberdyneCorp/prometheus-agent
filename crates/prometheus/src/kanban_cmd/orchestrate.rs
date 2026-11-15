//! `kanban swarm`, `kanban specify` and `kanban decompose` (spec: kanban,
//! Swarm topology helper; Orchestrator guidance, specify and decompose).

use std::fmt::Write as _;

use pmt_core::error::Error;
use pmt_kanban::orchestrate::SwarmSpec;
use pmt_kanban::{Filter, Status};
use serde_json::json;

use super::{Env, KanbanCommand};
use crate::kanban_host::orchestrate;

/// Runs one of these verbs; `None` when `command` is another's.
pub(super) fn run(env: &Env<'_>, command: &KanbanCommand) -> Option<Result<u8, Error>> {
    Some(match command {
        KanbanCommand::Swarm {
            goal,
            workers,
            verifier,
            synthesizer,
        } => swarm(env, goal, workers, verifier, synthesizer),
        KanbanCommand::Specify { id, all, tenant } => {
            specify(env, id.as_deref(), *all, tenant.as_deref())
        }
        KanbanCommand::Decompose { id } => decompose(env, id),
        _ => return None,
    })
}

fn swarm(
    env: &Env<'_>,
    goal: &str,
    workers: &[String],
    verifier: &str,
    synthesizer: &str,
) -> Result<u8, Error> {
    let mut board = env.board()?;
    let spec = SwarmSpec {
        goal,
        workers,
        verifier,
        synthesizer,
    };
    let swarm = board.swarm(&env.actor(), &spec, &env.profiles(), pmt_kanban::now_ms())?;
    if env.ctx.json {
        return env.document(
            "kanban swarm",
            &json!({"board": board.slug(), "root": swarm.root, "workers": swarm.workers, "verifier": swarm.verifier, "synthesizer": swarm.synthesizer}),
        );
    }
    let mut out = format!(
        "swarm on board {}\nroot (blackboard) {}\n",
        board.slug(),
        swarm.root
    );
    for (id, profile) in swarm.workers.iter().zip(workers) {
        let _ = writeln!(out, "worker {id} ({profile})");
    }
    let _ = writeln!(out, "verifier {} ({verifier})", swarm.verifier);
    let _ = writeln!(out, "synthesizer {} ({synthesizer})", swarm.synthesizer);
    env.raw(&out)
}

fn specify(env: &Env<'_>, id: Option<&str>, all: bool, tenant: Option<&str>) -> Result<u8, Error> {
    let mut board = env.board()?;
    let ids: Vec<String> = if all {
        board
            .tasks(&Filter {
                status: Some(Status::Triage),
                assignee: tenant.map(str::to_owned),
                archived: false,
            })?
            .into_iter()
            .map(|t| t.id)
            .collect()
    } else {
        id.map(str::to_owned).into_iter().collect()
    };
    let actor = env.actor();
    let mut out = String::new();
    let mut failed = false;
    for id in &ids {
        match orchestrate::specify(&env.scope, env.ctx.launch, &mut board, &actor, id) {
            Ok(()) => {
                let _ = writeln!(out, "ok {id} todo");
            }
            Err(err) => {
                failed = true;
                let _ = writeln!(out, "failed {id}: {}", err.message());
            }
        }
    }
    if ids.is_empty() {
        out.push_str("no triage tasks\n");
    }
    env.raw(&out)?;
    Ok(u8::from(failed))
}

fn decompose(env: &Env<'_>, id: &str) -> Result<u8, Error> {
    let mut board = env.board()?;
    let children =
        orchestrate::decompose(&env.scope, env.ctx.launch, &mut board, &env.actor(), id)?;
    if env.ctx.json {
        return env.document(
            "kanban decompose",
            &json!({"task_id": id, "children": children}),
        );
    }
    let mut out = format!("decomposed {id} into {} task(s)\n", children.len());
    for (key, child) in &children {
        let status = board
            .task(child)
            .map(|t| t.status.to_string())
            .unwrap_or_default();
        let _ = writeln!(out, "{child} ({key}) {status}");
    }
    env.raw(&out)
}

//! The kanban trigger: a task of kind `evolve` queues a job with
//! `trigger = "kanban"` and the task's id.

use std::path::Path;

use pmt_config::ProfileScope;
use pmt_config::schema::EvolveMode;
use pmt_core::error::Error;
use pmt_core::id::SessionId;
use pmt_evolve::job::{self, Request, TaskInput, Trigger};
use pmt_kanban::{Task, TaskKind};

/// Queues the job of evolve task `task` in its assignee's profile; the
/// job id. `target_task` is a session id, else a task file relative to
/// the task's workspace.
///
/// # Errors
///
/// `surface.not_included`, `evolve.disabled`, `cli.usage` (k),
/// `evolve.invalid_task`.
pub fn queue(scope: &ProfileScope, task: &Task, workspace: &Path) -> Result<String, Error> {
    crate::surface::require("evolve")?;
    let TaskKind::Evolve {
        target_task,
        mode,
        budget_k,
    } = &task.kind
    else {
        unreachable!("only evolve tasks queue evolution jobs");
    };
    let store = pmt_store::Store::open(scope)?;
    let input = match SessionId::parse(target_task) {
        Ok(id) => TaskInput::Session(id),
        Err(_) => TaskInput::File(workspace.join(target_task)),
    };
    let request = Request {
        k: Some(u8::try_from(*budget_k).unwrap_or(u8::MAX)),
        mode: Some(match mode {
            pmt_kanban::model::EvolveMode::Parallel => EvolveMode::Parallel,
            pmt_kanban::model::EvolveMode::Sequential => EvolveMode::Sequential,
        }),
        kanban_task_id: Some(task.id.clone()),
        ..Request::new(Trigger::Kanban, input)
    };
    job::queue(&super::profile(scope, Some(&store)), &request).map(|j| j.id)
}

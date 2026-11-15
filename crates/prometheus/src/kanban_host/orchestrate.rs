//! `specify` and `decompose` on the `kanban` auxiliary route (spec:
//! kanban, Orchestrator guidance, specify and decompose). Each call runs
//! in a hidden session tagged `kanban-<task>`, so relays see the affinity
//! key `kanban:<task-id>`. With `kanban.auto_decompose` the dispatcher
//! runs both before it dispatches: `specify` on triage ideas, `decompose`
//! on tasks assigned to an orchestrator profile. A specified task that
//! names no assignee goes to `kanban.orchestrator_profile` when set.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pmt_config::schema::KanbanRole;
use pmt_config::{Launch, Overrides, ProfileName, ProfileScope};
use pmt_core::env::LaunchEnv;
use pmt_core::error::{Error, codes};
use pmt_kanban::orchestrate::Graph;
use pmt_kanban::{Actor, Board, Boards, Filter, Status, Task};
use pmt_providers::auxiliary::AuxTask;
use pmt_store::{NewSession, Source, Store};

use super::assignees::Assignees;
use crate::chat::session::model::{Models, RouteFlags};

/// The orchestrator guidance a worker session of an orchestrator profile
/// gets in its frozen system prompt, after the worker guidance.
pub const ORCHESTRATOR_GUIDANCE: &str = "## Kanban orchestrator\n\
You coordinate; you do not implement. First discover the existing profiles (`prometheus kanban \
assignees` in the terminal, or the ones named in the task). Decompose the goal into small linked \
tasks with kanban_create, each assigned to an existing profile, with every shared decision (names, \
interfaces, formats, constraints) written into each child's body so it stands alone; link \
dependencies with kanban_link. List the ids you created in kanban_complete's created_tasks.";

const SPECIFY: &str = "You turn a one-line kanban task idea into a complete task body for a \
worker: the context, what to do, what not to do, and a final section headed `Acceptance criteria:` \
with a bulleted, checkable list. Answer with the body only.";

/// Characters of a reply kept as a body.
const BODY_CHARS: usize = 8_000;

fn decompose_system(assignees: &[String]) -> String {
    format!(
        "You are a kanban orchestrator. Decompose the goal below into a small graph of tasks for \
         these existing assignees only: {}. Write every shared decision into each task's body so \
         it stands alone. Answer with one JSON object: {{\"tasks\": [{{\"key\": \"short-id\", \
         \"title\": \"...\", \"body\": \"...\", \"assignee\": \"<one of the assignees>\", \
         \"depends_on\": [\"<keys>\"]}}]}} and nothing else.",
        assignees.join(", ")
    )
}

fn task_text(task: &Task) -> String {
    format!("Task {}: {}\n\n{}", task.id, task.title, task.body.trim())
}

/// The home's profiles and `scope`'s lanes, for the decomposition prompt.
#[must_use]
pub fn assignee_names(scope: &ProfileScope) -> Vec<String> {
    let mut names = vec![pmt_config::layout::DEFAULT_PROFILE.to_owned()];
    if let Ok(entries) = std::fs::read_dir(scope.home().join("profiles")) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry.path().is_dir()
                && super::profile_exists(scope.home(), &name)
                && !names.contains(&name)
            {
                names.push(name);
            }
        }
    }
    names.sort();
    names.extend(
        super::lanes::declared(scope)
            .into_iter()
            .map(|l| l.lane.name),
    );
    names
}

/// One answer of the `kanban` route for `task`.
fn ask(
    scope: &ProfileScope,
    launch: &LaunchEnv,
    task: &str,
    system: &str,
    user: &str,
) -> Result<String, Error> {
    let store = Arc::new(Store::open(scope)?);
    let session = store.create_session(&NewSession {
        source_tag: Some(format!(
            "{}-{task}",
            crate::chat::session::KANBAN_SOURCE_TAG
        )),
        title: Some(format!("kanban {task}")),
        hidden: true,
        ..NewSession::new(Source::Cli)
    })?;
    let models = Models::open(
        scope,
        &store,
        launch.user_home().map(PathBuf::as_path),
        &RouteFlags::default(),
        None,
    )?;
    let reply = crate::model_cmd::block_on(models.ask_auxiliary(
        AuxTask::Kanban,
        session.id,
        system,
        user,
        4_000,
    ))??;
    reply.filter(|r| !r.trim().is_empty()).ok_or_else(|| {
        Error::new(
            &codes::PROVIDER_CALL_FAILED,
            "the kanban model gave no answer",
        )
    })
}

/// Specifies triage task `id`: its body becomes the model's specification
/// and it moves to `todo`.
///
/// # Errors
///
/// `kanban.illegal_transition` outside triage, model errors.
pub fn specify(
    scope: &ProfileScope,
    launch: &LaunchEnv,
    board: &mut Board,
    actor: &Actor,
    id: &str,
) -> Result<(), Error> {
    let task = board.task(id)?;
    if task.status != Status::Triage {
        return Err(pmt_kanban::workflow::illegal(
            actor.kind,
            task.status,
            Status::Todo,
        ));
    }
    let body = ask(scope, launch, id, SPECIFY, &task_text(&task))?;
    let body: String = body.trim().chars().take(BODY_CHARS).collect();
    board.specify(actor, id, &body, pmt_kanban::now_ms())?;
    match orchestrator_profile(scope) {
        Some(profile) if task.assignee.is_none() => board.assign(
            actor,
            id,
            Some(&profile),
            &Assignees::of(scope),
            pmt_kanban::now_ms(),
        ),
        _ => Ok(()),
    }
}

/// `kanban.orchestrator_profile` when it names an existing profile: the
/// assignee a specified triage task without one gets. An unknown name is
/// ignored rather than failing every specify.
#[must_use]
pub fn orchestrator_profile(scope: &ProfileScope) -> Option<String> {
    scope
        .config()
        .kanban
        .orchestrator_profile
        .clone()
        .filter(|name| super::profile_exists(scope.home(), name))
}

/// Decomposes task `id` into a validated child graph created under it.
///
/// # Errors
///
/// `kanban.invalid_task` for a graph that does not check (nothing is
/// created), model errors.
pub fn decompose(
    scope: &ProfileScope,
    launch: &LaunchEnv,
    board: &mut Board,
    actor: &Actor,
    id: &str,
) -> Result<std::collections::BTreeMap<String, String>, Error> {
    let task = board.task(id)?;
    let system = decompose_system(&assignee_names(scope));
    let reply = ask(scope, launch, id, &system, &task_text(&task))?;
    let graph = Graph::parse(&reply)?;
    let auto_promote = scope.config().kanban.auto_promote_children;
    board.decompose(
        actor,
        id,
        &graph,
        auto_promote,
        &Assignees::of(scope),
        pmt_kanban::now_ms(),
    )
}

/// Whether `profile` of the home at `home` has `kanban.role = orchestrator`.
#[must_use]
pub fn is_orchestrator(home: &Path, profile: &str) -> bool {
    let Ok(home) = pmt_config::layout::Home::open(home) else {
        return false;
    };
    ProfileName::parse(profile)
        .and_then(|n| home.existing_profile(&n))
        .and_then(|dir| ProfileScope::open(dir, &Overrides::default(), Launch::Other))
        .is_ok_and(|s| s.config().kanban.role == Some(KanbanRole::Orchestrator))
}

fn seen(board: &Board, id: &str, kind: &str) -> bool {
    board.events(Some(id)).is_ok_and(|events| {
        events
            .iter()
            .any(|e| e.kind == kind || e.kind == format!("{kind}_failed"))
    })
}

/// `kanban.auto_decompose`: specify every triage task and decompose every
/// task assigned to an orchestrator (once each) before the tick.
pub fn auto(scope: &ProfileScope, boards: &Boards, launch: &LaunchEnv) {
    let actor = Actor::dispatcher();
    for slug in boards.live().unwrap_or_default() {
        let Ok(mut board) = boards.board(&slug) else {
            continue;
        };
        let tasks = board.tasks(&Filter::default()).unwrap_or_default();
        for task in tasks {
            let (kind, result) = match task.status {
                Status::Triage if !seen(&board, &task.id, "specified") => (
                    "specified",
                    specify(scope, launch, &mut board, &actor, &task.id),
                ),
                Status::Todo | Status::Ready
                    if task
                        .assignee
                        .as_deref()
                        .is_some_and(|a| is_orchestrator(scope.home(), a))
                        && !seen(&board, &task.id, "decomposed") =>
                {
                    (
                        "decomposed",
                        decompose(scope, launch, &mut board, &actor, &task.id).map(|_| ()),
                    )
                }
                _ => continue,
            };
            if let Err(err) = result {
                tracing::warn!(
                    component = "kanban",
                    task = task.id.as_str(),
                    "auto {kind}: {}",
                    err.message()
                );
                let payload = serde_json::json!({"reason": err.message()});
                let _ = board.record(
                    Some(&task.id),
                    None,
                    &actor,
                    &format!("{kind}_failed"),
                    &payload,
                    pmt_kanban::now_ms(),
                );
            }
        }
    }
}

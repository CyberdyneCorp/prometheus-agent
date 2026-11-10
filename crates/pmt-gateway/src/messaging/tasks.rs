//! Background tasks started from a chat (spec: messaging-gateway,
//! Background sessions from chat; subagent-delegation, User-launched
//! background sessions). `/background <prompt>` (alias `/bg`) runs the
//! prompt in a session of its own: the same profile, the chat's policy
//! (model and toolsets), the chat as its origin and none of the
//! conversation's history. The chat is answered at once with the task's
//! number and stays free for its own turns; the task waits for a slot of
//! `delegation.max_concurrent_children` (the host's admission), and its
//! result (or failure) is delivered to the chat and thread that asked,
//! labelled with its number, never into the chat's own session. A chat
//! runs at most `gateway.background.max_per_chat` tasks at once; `/agents`
//! in the chat lists and stops them (the process's task registry).

use std::collections::HashMap;
use std::sync::Mutex;

use pmt_rpc::contracts::{SubmitParams, TurnFinished};
use pmt_store::gateway::outbox::{NewOutbound, OutboundKind};

use std::sync::Arc;

use pmt_core::tasks::{NewTask, Registry, State, complete, failed, started, too_many};

use super::Shared;
use super::agents::Conversation;
use super::key::Routing;
use crate::host::Author;

/// One running task.
#[derive(Debug, Clone)]
struct Task {
    entry: Arc<pmt_core::tasks::Task>,
    profile: String,
    routing: Routing,
}

/// The running tasks, by their session, and the concurrency slots they
/// hold.
#[derive(Default)]
pub(crate) struct Tasks(
    Mutex<HashMap<String, Task>>,
    Mutex<HashMap<String, crate::messaging::agents::BackgroundSlot>>,
);

impl std::fmt::Debug for Tasks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tasks").finish_non_exhaustive()
    }
}

/// The chat a routing belongs to (tasks are counted per chat and thread).
fn chat_of(routing: &Routing) -> (String, String, String, Option<String>) {
    (
        routing.profile.clone(),
        routing.platform.clone(),
        routing.chat_id.clone(),
        routing.thread_id.clone(),
    )
}

/// What finished task `number` tells its chat, and its end state.
pub(crate) fn outcome(number: u32, finished: &TurnFinished) -> (String, State) {
    if finished.exit_reason == "completed" {
        let answer = finished.final_response.as_deref().unwrap_or_default();
        return (complete(number, answer), State::Completed);
    }
    if finished.exit_reason.starts_with("interrupted") {
        return (failed(number, "stopped"), State::Interrupted);
    }
    let reason = finished.error.as_ref().map_or_else(
        || finished.exit_reason.replace('_', " "),
        |e| e.message.clone(),
    );
    (failed(number, &reason), State::Failed(reason))
}

impl Tasks {
    /// Tasks running for `routing`'s chat.
    fn running_in(&self, routing: &Routing) -> usize {
        let chat = chat_of(routing);
        self.0
            .lock()
            .expect("tasks poisoned")
            .values()
            .filter(|t| chat_of(&t.routing) == chat)
            .count()
    }

    fn take(&self, session: &str) -> Option<Task> {
        self.1.lock().expect("tasks poisoned").remove(session);
        self.0.lock().expect("tasks poisoned").remove(session)
    }

    /// Keeps `slot` while the task of `session` runs.
    fn hold(&self, session: &str, slot: crate::messaging::agents::BackgroundSlot) {
        if self.0.lock().expect("tasks poisoned").contains_key(session) {
            self.1
                .lock()
                .expect("tasks poisoned")
                .insert(session.to_owned(), slot);
        }
    }

    /// Whether `session` runs a background task.
    pub(crate) fn owns(&self, session: &str) -> bool {
        self.0.lock().expect("tasks poisoned").contains_key(session)
    }
}

/// Starts `prompt` as a background task of `routing`'s chat (whose
/// current session is `launcher`); the text to answer the chat with (the
/// start notice or the refusal).
pub(crate) async fn start(
    shared: &Arc<Shared>,
    routing: &Routing,
    policy: Option<pmt_config::schema::ChatPolicy>,
    prompt: &str,
    launcher: Option<&str>,
) -> String {
    let prompt = prompt.trim();
    if prompt.is_empty() {
        return "Usage: /background <prompt>".to_owned();
    }
    let max = shared
        .agents
        .scope(&routing.profile)
        .map_or(3, |s| s.config().gateway.background.max_per_chat);
    if shared.tasks.running_in(routing) >= max as usize {
        return too_many(max);
    }
    let conversation = Conversation {
        platform: routing.platform.clone(),
        chat_id: routing.chat_id.clone(),
        thread_id: routing.thread_id.clone(),
        user_id: routing.sender_id.clone(),
        parent: launcher.map(str::to_owned),
        background: true,
        policy,
    };
    let session = match shared
        .agents
        .create_conversation(&routing.profile, conversation)
        .await
    {
        Ok(session) => session,
        Err(err) => return format!("❌ Background task failed: {}", err.message()),
    };
    let launcher = launcher.map_or_else(
        || format!("chat:{}:{}", routing.platform, routing.chat_id),
        str::to_owned,
    );
    let entry = register(shared, &routing.profile, &session, launcher, prompt);
    shared.tasks.0.lock().expect("tasks poisoned").insert(
        session.clone(),
        Task {
            entry: Arc::clone(&entry),
            profile: routing.profile.clone(),
            routing: routing.clone(),
        },
    );
    super::peer::watch(shared, &routing.profile, &session);
    tracing::info!(
        profile = routing.profile,
        task = entry.id,
        "background task started"
    );
    let me = Arc::clone(shared);
    let (routing, text) = (routing.clone(), prompt.to_owned());
    pmt_core::spawn::spawn(async move { run(&me, &routing, &session, text).await });
    started(entry.number, prompt, &entry.id)
}

/// Registers the task of `session` in the process's registry; stopping it
/// interrupts the session's turn.
fn register(
    shared: &Arc<Shared>,
    profile: &str,
    session: &str,
    launcher: String,
    prompt: &str,
) -> Arc<pmt_core::tasks::Task> {
    let agents = Arc::clone(&shared.agents);
    let (owner, id) = (profile.to_owned(), session.to_owned());
    let runtime = tokio::runtime::Handle::current();
    let model = shared
        .agents
        .scope(profile)
        .and_then(|s| s.config().model.default.clone())
        .unwrap_or_default();
    let entry = Registry::process().add(NewTask {
        id: pmt_core::id::BackgroundId::new().to_string(),
        launcher,
        prompt: prompt.to_owned(),
        model,
        stop: Box::new(move || {
            let (agents, owner, id) = (Arc::clone(&agents), owner.clone(), id.clone());
            pmt_core::spawn::spawn_on(&runtime, async move {
                if let Err(err) = agents.interrupt(&owner, &id).await {
                    tracing::warn!(error = %err, "background task not interrupted");
                }
            });
        }),
    });
    entry.set_session(session);
    entry
}

/// Waits for the task's slot, then submits its prompt; a failure to
/// submit ends the task at once.
async fn run(shared: &Arc<Shared>, routing: &Routing, session: &str, prompt: String) {
    let slot = shared
        .agents
        .admit_background(&routing.profile, session)
        .await;
    let submitted = match slot {
        Ok(slot) => {
            if let Some(task) = shared.tasks.0.lock().expect("tasks poisoned").get(session) {
                task.entry.running();
            }
            shared.tasks.hold(session, slot);
            submit(shared, routing, session, prompt).await
        }
        Err(err) => Err(err),
    };
    if let Err(err) = submitted {
        end(shared, session, &failed_turn(&err));
    }
}

async fn submit(
    shared: &Arc<Shared>,
    routing: &Routing,
    session: &str,
    prompt: String,
) -> Result<(), pmt_core::Error> {
    let params = SubmitParams {
        session_id: session.to_owned(),
        text: prompt,
        images: Vec::new(),
        files: Vec::new(),
        run_budget_secs: None,
        input_id: None,
        author: None,
    };
    let author = Author {
        client_kind: "gateway".to_owned(),
        client_id: format!("{}:{}", routing.platform, routing.sender_id),
        principal: routing.profile.clone(),
        sent_at: Some(jiff::Timestamp::now().to_string()),
        prefix: None,
        origin: None,
    };
    let link = shared.server.link(session);
    shared
        .agents
        .submit(&routing.profile, params, author, link)
        .await
        .map(|_| ())
}

/// A turn that failed before it ran.
fn failed_turn(err: &pmt_core::Error) -> TurnFinished {
    TurnFinished {
        turn_id: String::new(),
        exit_reason: "error".to_owned(),
        final_response: None,
        error: Some(pmt_rpc::contracts::TurnError::of(err)),
        usage: None,
        report: None,
    }
}

/// A turn of a task's session ended: its chat is told how it went.
pub(crate) fn finished(shared: &Shared, session: &str, finished: &TurnFinished) {
    end(shared, session, finished);
}

fn end(shared: &Shared, session: &str, finished: &TurnFinished) {
    let Some(task) = shared.tasks.take(session) else {
        return;
    };
    let (body, state) = outcome(task.entry.number, finished);
    tracing::info!(
        profile = task.profile,
        task = task.entry.id,
        exit = finished.exit_reason,
        "background task finished"
    );
    task.entry.finish(state);
    let routing = &task.routing;
    let new = NewOutbound {
        idempotency_key: format!("background:{}", task.entry.id),
        session_id: Some(session.to_owned()),
        platform: routing.platform.clone(),
        chat_id: routing.chat_id.clone(),
        thread_id: routing.thread_id.clone(),
        reply_to: None,
        edit_of: None,
        kind: OutboundKind::Notification,
        body,
        attachments: Vec::new(),
        notify: None,
    };
    shared.deliver(&task.profile, &new);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ended(reason: &str, answer: Option<&str>) -> TurnFinished {
        TurnFinished {
            turn_id: "t".into(),
            exit_reason: reason.into(),
            final_response: answer.map(str::to_owned),
            error: None,
            usage: None,
            report: None,
        }
    }

    #[test]
    fn the_outcome_names_success_or_the_reason() {
        assert_eq!(
            outcome(2, &ended("completed", Some("done"))).0,
            "✅ Background task #2 complete\n\ndone"
        );
        assert_eq!(
            outcome(1, &ended("budget_exhausted", None)).0,
            "❌ Background task #1 failed: budget exhausted"
        );
    }
}

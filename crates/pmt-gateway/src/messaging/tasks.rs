//! Background tasks started from a chat (spec: messaging-gateway,
//! Background sessions from chat). `/background <prompt>` (alias `/bg`)
//! runs the prompt in a session of its own: the same profile, the chat's
//! policy (model and toolsets), the chat as its origin and none of the
//! conversation's history. The chat is answered at once and stays free
//! for its own turns; the task's result (or failure) is delivered to the
//! chat and thread that asked. A chat runs at most
//! `gateway.background.max_per_chat` tasks at once.

use std::collections::HashMap;
use std::sync::Mutex;

use pmt_rpc::contracts::{SubmitParams, TurnFinished};
use pmt_store::gateway::outbox::{NewOutbound, OutboundKind};

use pmt_core::tasks::{complete, failed, started, too_many};

use super::Shared;
use super::agents::Conversation;
use super::key::Routing;
use crate::host::Author;

/// One running task.
#[derive(Debug, Clone)]
struct Task {
    id: String,
    profile: String,
    routing: Routing,
}

/// The running tasks, by their session.
#[derive(Debug, Default)]
pub(crate) struct Tasks(Mutex<HashMap<String, Task>>);

/// The chat a routing belongs to (tasks are counted per chat and thread).
fn chat_of(routing: &Routing) -> (String, String, String, Option<String>) {
    (
        routing.profile.clone(),
        routing.platform.clone(),
        routing.chat_id.clone(),
        routing.thread_id.clone(),
    )
}

/// What a finished task tells its chat.
pub(crate) fn outcome(finished: &TurnFinished) -> String {
    if finished.exit_reason == "completed" {
        return complete(finished.final_response.as_deref().unwrap_or_default());
    }
    let reason = finished.error.as_ref().map_or_else(
        || finished.exit_reason.replace('_', " "),
        |e| e.message.clone(),
    );
    failed(&reason)
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
        self.0.lock().expect("tasks poisoned").remove(session)
    }

    /// Whether `session` runs a background task.
    pub(crate) fn owns(&self, session: &str) -> bool {
        self.0.lock().expect("tasks poisoned").contains_key(session)
    }
}

/// Starts `prompt` as a background task of `routing`'s chat; the text to
/// answer the chat with (the start notice or the refusal).
pub(crate) async fn start(
    shared: &Shared,
    routing: &Routing,
    policy: Option<pmt_config::schema::ChatPolicy>,
    prompt: &str,
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
        parent: None,
        policy,
    };
    let session = match shared
        .agents
        .create_conversation(&routing.profile, conversation)
        .await
    {
        Ok(session) => session,
        Err(err) => return failed(err.message()),
    };
    let id = pmt_core::id::BackgroundId::new().to_string();
    shared.tasks.0.lock().expect("tasks poisoned").insert(
        session.clone(),
        Task {
            id: id.clone(),
            profile: routing.profile.clone(),
            routing: routing.clone(),
        },
    );
    super::peer::watch(shared, &routing.profile, &session);
    let params = SubmitParams {
        session_id: session.clone(),
        text: prompt.to_owned(),
        images: Vec::new(),
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
    };
    let link = shared.server.link(&session);
    if let Err(err) = shared
        .agents
        .submit(&routing.profile, params, author, link)
        .await
    {
        shared.tasks.take(&session);
        return failed(err.message());
    }
    tracing::info!(
        profile = routing.profile,
        task = id,
        "background task started"
    );
    started(prompt, &id)
}

/// A turn of a task's session ended: its chat is told how it went.
pub(crate) fn finished(shared: &Shared, session: &str, finished: &TurnFinished) {
    let Some(task) = shared.tasks.take(session) else {
        return;
    };
    tracing::info!(
        profile = task.profile,
        task = task.id,
        exit = finished.exit_reason,
        "background task finished"
    );
    let routing = &task.routing;
    let new = NewOutbound {
        idempotency_key: format!("background:{}", task.id),
        session_id: Some(session.to_owned()),
        platform: routing.platform.clone(),
        chat_id: routing.chat_id.clone(),
        thread_id: routing.thread_id.clone(),
        reply_to: None,
        edit_of: None,
        kind: OutboundKind::Notification,
        body: outcome(finished),
        attachments: Vec::new(),
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
            outcome(&ended("completed", Some("done"))),
            "✅ Background task complete\n\ndone"
        );
        assert_eq!(
            outcome(&ended("budget_exhausted", None)),
            "❌ Background task failed: budget exhausted"
        );
    }
}

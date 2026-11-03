//! Slack's streaming message API (spec: messaging-gateway, Native platform
//! streaming and task cards): `chat.startStream` opens a message in a
//! thread (a DM message's own `ts` when it is not in one),
//! `chat.appendStream` adds Markdown text or task-card chunks
//! (`task_update`, shown as a plan the turn works through) and
//! `chat.stopStream` ends it. `assistant.threads.setStatus` shows the
//! "is thinking…" line under the thread while the turn works.

use pmt_gateway::messaging::{Outgoing, SendError, TaskCard, TaskStatus};
use serde_json::json;

use crate::api::Api;

/// Opens a stream answering `message`; its `ts`.
pub(crate) async fn start(api: &Api, message: &Outgoing) -> Result<String, SendError> {
    let thread = message
        .thread_id
        .as_deref()
        .or(message.reply_to.as_deref())
        .ok_or(SendError::Unsupported)?;
    let opened = api
        .call(
            "chat.startStream",
            &json!({"channel": message.chat_id, "thread_ts": thread, "task_display_mode": "plan"}),
        )
        .await?;
    opened["ts"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| SendError::Server("Slack gave the stream no ts".to_owned()))
}

/// Appends Markdown `text`.
pub(crate) async fn append(
    api: &Api,
    channel: &str,
    ts: &str,
    text: &str,
) -> Result<(), SendError> {
    api.call(
        "chat.appendStream",
        &json!({"channel": channel, "ts": ts, "markdown_text": text}),
    )
    .await
    .map(drop)
}

fn status_of(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Running => "in_progress",
        TaskStatus::Done => "complete",
        TaskStatus::Failed => "error",
    }
}

/// Adds or updates task card `task`.
pub(crate) async fn task(
    api: &Api,
    channel: &str,
    ts: &str,
    task: &TaskCard,
) -> Result<(), SendError> {
    let chunk = json!({"type": "task_update", "id": task.id, "title": task.title,
                       "status": status_of(task.status)});
    api.call(
        "chat.appendStream",
        &json!({"channel": channel, "ts": ts, "chunks": [chunk]}),
    )
    .await
    .map(drop)
}

/// Ends the stream.
pub(crate) async fn stop(api: &Api, channel: &str, ts: &str) -> Result<(), SendError> {
    api.call("chat.stopStream", &json!({"channel": channel, "ts": ts}))
        .await
        .map(drop)
}

/// Shows (or, with `None`, clears) the thread's working status.
pub(crate) async fn status(
    api: &Api,
    channel: &str,
    thread: Option<&str>,
    status: Option<&str>,
) -> Result<(), SendError> {
    let thread = thread.ok_or(SendError::Unsupported)?;
    api.call(
        "assistant.threads.setStatus",
        &json!({"channel_id": channel, "thread_ts": thread, "status": status.unwrap_or_default()}),
    )
    .await
    .map(drop)
}

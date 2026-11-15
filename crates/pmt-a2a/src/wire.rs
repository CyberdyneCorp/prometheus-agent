//! A2A wire shapes in both dialects (spec: agent-interop, A2A JSON-RPC
//! methods and task lifecycle). A2A v1.0 is proto JSON: Pascal-case methods
//! (`SendMessage`), `TASK_STATE_*`/`ROLE_*` enum names, parts as
//! `{"text": ...}` and results wrapped in a oneof (`{"task": ...}`). The
//! pre-1.0 (v0.3) aliases (`message/send`, `tasks/get`, ...) use lowercase
//! states and roles, `kind`-tagged parts and objects, and bare results.
//! The server answers each request in the dialect of the method it called;
//! everything read from a peer is parsed leniently in either dialect.

use serde_json::{Map, Value, json};

/// The JSON-RPC version string.
pub const JSONRPC: &str = "2.0";
/// The protocol version this implementation speaks.
pub const PROTOCOL_VERSION: &str = "1.0";

/// JSON-RPC and A2A error codes.
pub mod code {
    /// The body is not JSON.
    pub const PARSE: i64 = -32700;
    /// Not a JSON-RPC request.
    pub const INVALID_REQUEST: i64 = -32600;
    /// No such method.
    pub const METHOD_NOT_FOUND: i64 = -32601;
    /// Bad parameters.
    pub const INVALID_PARAMS: i64 = -32602;
    /// Something failed inside.
    pub const INTERNAL: i64 = -32603;
    /// No task has the id.
    pub const TASK_NOT_FOUND: i64 = -32001;
    /// The task already ended.
    pub const TASK_NOT_CANCELABLE: i64 = -32002;
    /// The push configuration was refused.
    pub const PUSH_NOT_SUPPORTED: i64 = -32003;
    /// The operation is not supported.
    pub const UNSUPPORTED: i64 = -32004;
    /// The caller exceeded its rate limit (server-defined).
    pub const RATE_LIMITED: i64 = -32029;
}

/// Which protocol generation a request used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    /// A2A v1.0 (proto JSON).
    V1,
    /// The pre-1.0 aliases (v0.3 JSON).
    V03,
}

/// A JSON-RPC method of the A2A server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// Send a message, answer with the task.
    SendMessage,
    /// Send a message, answer with an event stream.
    SendStreamingMessage,
    /// One task.
    GetTask,
    /// The caller's tasks.
    ListTasks,
    /// Cancel a task.
    CancelTask,
    /// Stream a running task's events.
    SubscribeToTask,
    /// Register a push-notification configuration.
    CreatePush,
    /// One push-notification configuration.
    GetPush,
    /// A task's push-notification configurations.
    ListPush,
    /// Remove a push-notification configuration.
    DeletePush,
}

/// The method `name` names and its dialect; `None` for an unknown method
/// (answered `-32601`).
#[must_use]
pub fn method(name: &str) -> Option<(Method, Dialect)> {
    use Dialect::{V1, V03};
    Some(match name {
        "SendMessage" => (Method::SendMessage, V1),
        "message/send" => (Method::SendMessage, V03),
        "SendStreamingMessage" => (Method::SendStreamingMessage, V1),
        "message/stream" => (Method::SendStreamingMessage, V03),
        "GetTask" => (Method::GetTask, V1),
        "tasks/get" => (Method::GetTask, V03),
        "ListTasks" => (Method::ListTasks, V1),
        "tasks/list" => (Method::ListTasks, V03),
        "CancelTask" => (Method::CancelTask, V1),
        "tasks/cancel" => (Method::CancelTask, V03),
        "SubscribeToTask" => (Method::SubscribeToTask, V1),
        "tasks/resubscribe" => (Method::SubscribeToTask, V03),
        "CreateTaskPushNotificationConfig" => (Method::CreatePush, V1),
        "tasks/pushNotificationConfig/set" => (Method::CreatePush, V03),
        "GetTaskPushNotificationConfig" => (Method::GetPush, V1),
        "tasks/pushNotificationConfig/get" => (Method::GetPush, V03),
        "ListTaskPushNotificationConfigs" | "ListTaskPushNotificationConfig" => {
            (Method::ListPush, V1)
        }
        "tasks/pushNotificationConfig/list" => (Method::ListPush, V03),
        "DeleteTaskPushNotificationConfig" => (Method::DeletePush, V1),
        "tasks/pushNotificationConfig/delete" => (Method::DeletePush, V03),
        _ => return None,
    })
}

/// Where a task is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    /// Accepted, not started.
    Submitted,
    /// The agent works on it.
    Working,
    /// The agent needs more input.
    InputRequired,
    /// The agent replied.
    Completed,
    /// Cancelled.
    Canceled,
    /// The turn failed.
    Failed,
    /// Refused without a turn (untrusted peer, ping-pong cap).
    Rejected,
    /// The agent needs authentication.
    AuthRequired,
}

const STATES: [(TaskState, &str, &str); 8] = [
    (TaskState::Submitted, "TASK_STATE_SUBMITTED", "submitted"),
    (TaskState::Working, "TASK_STATE_WORKING", "working"),
    (
        TaskState::InputRequired,
        "TASK_STATE_INPUT_REQUIRED",
        "input-required",
    ),
    (TaskState::Completed, "TASK_STATE_COMPLETED", "completed"),
    (TaskState::Canceled, "TASK_STATE_CANCELED", "canceled"),
    (TaskState::Failed, "TASK_STATE_FAILED", "failed"),
    (TaskState::Rejected, "TASK_STATE_REJECTED", "rejected"),
    (
        TaskState::AuthRequired,
        "TASK_STATE_AUTH_REQUIRED",
        "auth-required",
    ),
];

impl TaskState {
    /// The state's name in `dialect`.
    #[must_use]
    pub fn wire(self, dialect: Dialect) -> &'static str {
        let (_, v1, v03) = STATES
            .iter()
            .find(|(s, _, _)| *s == self)
            .copied()
            .unwrap_or(STATES[0]);
        match dialect {
            Dialect::V1 => v1,
            Dialect::V03 => v03,
        }
    }

    /// The lowercase name (`completed`, `input-required`).
    #[must_use]
    pub fn name(self) -> &'static str {
        self.wire(Dialect::V03)
    }

    /// Parses a state in either dialect.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        STATES
            .iter()
            .find(|(_, v1, v03)| *v1 == text || *v03 == text)
            .map(|(s, _, _)| *s)
    }

    /// Whether the task ended.
    #[must_use]
    pub fn terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Canceled | Self::Failed | Self::Rejected
        )
    }
}

/// The text of a list of parts in either dialect: text parts as they
/// are, data parts as JSON, file parts by name or URL.
#[must_use]
pub fn parts_text(parts: &Value) -> String {
    let Some(parts) = parts.as_array() else {
        return String::new();
    };
    parts
        .iter()
        .filter_map(part_text)
        .collect::<Vec<_>>()
        .join("\n")
}

fn part_text(part: &Value) -> Option<String> {
    if let Some(text) = part["text"].as_str() {
        return Some(text.to_owned());
    }
    if !part["data"].is_null() {
        return Some(part["data"].to_string());
    }
    let file = if part["file"].is_object() {
        &part["file"]
    } else {
        part
    };
    ["url", "uri", "filename", "name"]
        .iter()
        .find_map(|key| file[*key].as_str())
        .map(|name| format!("[file: {name}]"))
}

/// What an incoming `message` says.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Incoming {
    /// Its text.
    pub text: String,
    /// The caller's message id.
    pub message_id: Option<String>,
    /// The conversation it continues.
    pub context_id: Option<String>,
    /// The task it continues (an `input-required` one).
    pub task_id: Option<String>,
}

impl Incoming {
    /// Reads `params.message` of a send request.
    #[must_use]
    pub fn of(params: &Value) -> Option<Self> {
        let message = params.get("message")?;
        let text = |key: &str| {
            message[key]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        };
        Some(Self {
            text: parts_text(&message["parts"]),
            message_id: text("messageId"),
            context_id: text("contextId"),
            task_id: text("taskId"),
        })
    }
}

/// `text` as one text part.
#[must_use]
pub fn text_parts(text: &str, dialect: Dialect) -> Value {
    match dialect {
        Dialect::V1 => json!([{"text": text}]),
        Dialect::V03 => json!([{"kind": "text", "text": text}]),
    }
}

/// One message of a conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    /// Whether the agent said it (else the caller).
    pub agent: bool,
    /// Its id.
    pub message_id: String,
    /// Its text.
    pub text: String,
}

/// A message object.
#[must_use]
pub fn message(turn: &Turn, context_id: &str, task_id: &str, dialect: Dialect) -> Value {
    let role = match (dialect, turn.agent) {
        (Dialect::V1, true) => "ROLE_AGENT",
        (Dialect::V1, false) => "ROLE_USER",
        (Dialect::V03, true) => "agent",
        (Dialect::V03, false) => "user",
    };
    let mut out = json!({
        "messageId": turn.message_id,
        "contextId": context_id,
        "taskId": task_id,
        "role": role,
        "parts": text_parts(&turn.text, dialect),
    });
    if dialect == Dialect::V03 {
        out["kind"] = "message".into();
    }
    out
}

/// What a task object shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskView<'a> {
    /// The task id.
    pub id: &'a str,
    /// Its conversation.
    pub context_id: &'a str,
    /// Its state.
    pub state: TaskState,
    /// A status message (why it was rejected or failed).
    pub status_text: Option<&'a str>,
    /// The agent's reply (the task's artifact).
    pub reply: Option<&'a str>,
    /// Its messages, oldest first.
    pub history: &'a [Turn],
    /// When the state last changed (RFC 3339).
    pub timestamp: &'a str,
}

/// The artifact id of a task's reply.
#[must_use]
pub fn artifact_id(task_id: &str) -> String {
    format!("{task_id}-reply")
}

fn status(view: &TaskView<'_>, dialect: Dialect) -> Value {
    let mut status = json!({
        "state": view.state.wire(dialect),
        "timestamp": view.timestamp,
    });
    if let Some(text) = view.status_text {
        let turn = Turn {
            agent: true,
            message_id: format!("{}-status", view.id),
            text: text.to_owned(),
        };
        status["message"] = message(&turn, view.context_id, view.id, dialect);
    }
    status
}

fn artifact(task_id: &str, reply: &str, dialect: Dialect) -> Value {
    json!({
        "artifactId": artifact_id(task_id),
        "name": "reply",
        "parts": text_parts(reply, dialect),
    })
}

/// A task object, with at most `history_length` latest messages (`None`:
/// all).
#[must_use]
pub fn task(view: &TaskView<'_>, dialect: Dialect, history_length: Option<usize>) -> Value {
    let skip = history_length.map_or(0, |n| view.history.len().saturating_sub(n));
    let history: Vec<Value> = view.history[skip..]
        .iter()
        .map(|t| message(t, view.context_id, view.id, dialect))
        .collect();
    let mut out = json!({
        "id": view.id,
        "contextId": view.context_id,
        "status": status(view, dialect),
        "history": history,
        "artifacts": view.reply.map_or_else(Vec::new, |r| vec![artifact(view.id, r, dialect)]),
    });
    if dialect == Dialect::V03 {
        out["kind"] = "task".into();
    }
    out
}

/// The result of `SendMessage`/`GetTask`-style calls carrying a task:
/// wrapped in v1.0 send results, bare otherwise.
#[must_use]
pub fn send_result(task: Value, dialect: Dialect) -> Value {
    match dialect {
        Dialect::V1 => json!({"task": task}),
        Dialect::V03 => task,
    }
}

/// A status-update stream event.
#[must_use]
pub fn status_update(view: &TaskView<'_>, dialect: Dialect) -> Value {
    let body = json!({
        "taskId": view.id,
        "contextId": view.context_id,
        "status": status(view, dialect),
    });
    match dialect {
        Dialect::V1 => json!({"statusUpdate": body}),
        Dialect::V03 => {
            let mut body = body;
            body["kind"] = "status-update".into();
            body["final"] = view.state.terminal().into();
            body
        }
    }
}

/// An artifact-update stream event carrying the reply.
#[must_use]
pub fn artifact_update(task_id: &str, context_id: &str, reply: &str, dialect: Dialect) -> Value {
    let body = json!({
        "taskId": task_id,
        "contextId": context_id,
        "artifact": artifact(task_id, reply, dialect),
        "append": false,
        "lastChunk": true,
    });
    match dialect {
        Dialect::V1 => json!({"artifactUpdate": body}),
        Dialect::V03 => {
            let mut body = body;
            body["kind"] = "artifact-update".into();
            body
        }
    }
}

/// A JSON-RPC success response.
#[must_use]
pub fn ok(id: &Value, result: Value) -> Value {
    let mut out = json!({"jsonrpc": JSONRPC, "id": id});
    out["result"] = result;
    out
}

/// A JSON-RPC error response.
#[must_use]
pub fn error(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": JSONRPC, "id": id, "error": {"code": code, "message": message}})
}

/// What a peer's answer said, read leniently in either dialect: a bare or
/// wrapped task, a bare or wrapped message, or a stream event.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Answer {
    /// The task id, when the answer was a task.
    pub task_id: Option<String>,
    /// The conversation.
    pub context_id: Option<String>,
    /// The task's state (`None` for a plain message).
    pub state: Option<TaskState>,
    /// The reply: the artifacts' text, else the status message's, else the
    /// message's.
    pub text: String,
}

impl Answer {
    /// Reads a JSON-RPC `result`.
    #[must_use]
    pub fn of(result: &Value) -> Self {
        let inner = ["task", "message", "statusUpdate", "artifactUpdate"]
            .iter()
            .find_map(|k| result.get(*k).filter(|v| v.is_object()))
            .unwrap_or(result);
        let text_of = |v: &Value, key: &str| v[key].as_str().map(str::to_owned);
        let state = inner["status"]["state"].as_str().and_then(TaskState::parse);
        let artifacts: Vec<String> = inner["artifacts"]
            .as_array()
            .map(|a| a.iter().map(|art| parts_text(&art["parts"])).collect())
            .unwrap_or_default();
        let artifact = parts_text(&inner["artifact"]["parts"]);
        let text = [
            artifacts.join("\n"),
            artifact,
            parts_text(&inner["status"]["message"]["parts"]),
            parts_text(&inner["parts"]),
        ]
        .into_iter()
        .find(|t| !t.is_empty())
        .unwrap_or_default();
        let is_task = state.is_some() || inner.get("artifacts").is_some();
        Self {
            task_id: if is_task {
                text_of(inner, "id").or_else(|| text_of(inner, "taskId"))
            } else {
                text_of(inner, "taskId")
            },
            context_id: text_of(inner, "contextId"),
            state,
            text,
        }
    }
}

/// A send request's params in `dialect`.
#[must_use]
pub fn send_params(text: &str, context_id: Option<&str>, dialect: Dialect) -> Value {
    let mut message = Map::new();
    message.insert(
        "messageId".into(),
        ulid::Ulid::generate().to_string().into(),
    );
    message.insert(
        "role".into(),
        match dialect {
            Dialect::V1 => "ROLE_USER",
            Dialect::V03 => "user",
        }
        .into(),
    );
    message.insert("parts".into(), text_parts(text, dialect));
    if let Some(context) = context_id {
        message.insert("contextId".into(), context.into());
    }
    if dialect == Dialect::V03 {
        message.insert("kind".into(), "message".into());
    }
    json!({"message": message})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_method_and_alias_resolves() {
        for (name, method_, dialect) in [
            ("SendMessage", Method::SendMessage, Dialect::V1),
            ("message/send", Method::SendMessage, Dialect::V03),
            ("message/stream", Method::SendStreamingMessage, Dialect::V03),
            ("tasks/get", Method::GetTask, Dialect::V03),
            ("tasks/cancel", Method::CancelTask, Dialect::V03),
            ("tasks/resubscribe", Method::SubscribeToTask, Dialect::V03),
            (
                "tasks/pushNotificationConfig/set",
                Method::CreatePush,
                Dialect::V03,
            ),
            ("ListTasks", Method::ListTasks, Dialect::V1),
        ] {
            assert_eq!(method(name), Some((method_, dialect)), "{name}");
        }
        assert_eq!(method("tasks/explode"), None);
    }

    #[test]
    fn answers_read_in_either_dialect() {
        let view = TaskView {
            id: "t1",
            context_id: "c1",
            state: TaskState::Completed,
            status_text: None,
            reply: Some("hello"),
            history: &[],
            timestamp: "2026-11-14T00:00:00Z",
        };
        for dialect in [Dialect::V1, Dialect::V03] {
            let answer = Answer::of(&send_result(task(&view, dialect, None), dialect));
            assert_eq!(answer.text, "hello");
            assert_eq!(answer.task_id.as_deref(), Some("t1"));
            assert_eq!(answer.state, Some(TaskState::Completed));
        }
        let message = json!({"message": {"role": "ROLE_AGENT", "parts": [{"text": "hi"}]}});
        assert_eq!(Answer::of(&message).text, "hi");
        assert_eq!(Answer::of(&message).state, None);
    }

    #[test]
    fn parts_read_in_either_dialect() {
        let parts = json!([{"text": "a"}, {"kind": "text", "text": "b"}, {"data": {"x": 1}}]);
        assert_eq!(parts_text(&parts), "a\nb\n{\"x\":1}");
    }
}

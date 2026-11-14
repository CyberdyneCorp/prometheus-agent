//! Turning the daemon's session events into ACP `session/update`s (spec:
//! acp-adapter, Reply projection is exactly-once; Every tool call reaches
//! a terminal status; Session metadata projection).
//!
//! Streamed answer text (`assistant.delta`) becomes agent message chunks
//! grouped by a per-segment `messageId`; a tool call starts a new segment;
//! the final `assistant.message` shows only what the stream has not shown
//! yet. Reasoning becomes thought chunks, the `todo` list a `plan`. Tool
//! calls are tracked from start to finish, so every call still open when
//! its turn ends is reported `failed`.

use std::collections::BTreeMap;

use pmt_rpc::contracts::{ASSISTANT_DELTA, COMMAND_NOTICE};
use serde_json::{Value, json};

/// What an unfinished tool call says when its turn ends.
pub(crate) const ENDED_WITHOUT_RESULT: &str =
    "This tool call ended without a result (blocked, denied, or interrupted).";

/// The ACP tool kind of each Prometheus tool; unmapped tools are `other`.
const KINDS: &[(&str, &str)] = &[
    ("read_file", "read"),
    ("write_file", "edit"),
    ("patch", "edit"),
    ("search_files", "search"),
    ("session_search", "search"),
    ("web_search", "search"),
    ("web_extract", "fetch"),
    ("web_fetch", "fetch"),
    ("terminal", "execute"),
    ("process", "execute"),
    ("execute_code", "execute"),
    ("todo", "think"),
    ("clarify", "think"),
];

/// The ACP kind of tool `tool`.
pub(crate) fn kind_of(tool: &str) -> &'static str {
    KINDS
        .iter()
        .find(|(name, _)| *name == tool)
        .map_or("other", |(_, kind)| kind)
}

/// A short title for a call: the tool and its main argument.
pub(crate) fn title_of(tool: &str, arguments: &Value) -> String {
    let main = ["path", "command", "query", "url", "code"]
        .iter()
        .find_map(|key| arguments.get(key).and_then(Value::as_str));
    match main {
        Some(main) => {
            let line = main.lines().next().unwrap_or_default();
            format!("{tool}: {}", line.chars().take(80).collect::<String>())
        }
        None => tool.to_owned(),
    }
}

/// One session's projection state.
#[derive(Debug, Default)]
pub(crate) struct Projector {
    /// Counts message segments; the current one's id is `msg-<n>`.
    segment: u64,
    /// The text streamed into the current segment.
    streamed: String,
    /// Tool calls started and not finished: id to tool name.
    open: BTreeMap<String, String>,
}

impl Projector {
    fn message_id(&self) -> String {
        format!("msg-{}", self.segment)
    }

    fn chunk(&self, kind: &str, text: &str) -> Value {
        json!({
            "sessionUpdate": kind,
            "messageId": self.message_id(),
            "content": {"type": "text", "text": text},
        })
    }

    /// The updates of one `session.event` envelope's data.
    pub(crate) fn event(&mut self, data: &Value) -> Vec<Value> {
        let payload = &data["payload"];
        match data["type"].as_str().unwrap_or_default() {
            ASSISTANT_DELTA => self.delta(payload["text"].as_str().unwrap_or_default()),
            "assistant.message" => self.final_text(payload["text"].as_str().unwrap_or_default()),
            "assistant.reasoning" => payload["text"]
                .as_str()
                .filter(|t| !t.is_empty())
                .map(|t| self.chunk("agent_thought_chunk", t))
                .into_iter()
                .collect(),
            "tool.call" => self.tool_call(payload),
            "tool.result" => self.tool_result(payload),
            COMMAND_NOTICE => payload["text"]
                .as_str()
                .map(|t| self.chunk("agent_message_chunk", t))
                .into_iter()
                .collect(),
            _ => Vec::new(),
        }
    }

    fn delta(&mut self, text: &str) -> Vec<Value> {
        if text.is_empty() {
            return Vec::new();
        }
        self.streamed.push_str(text);
        vec![self.chunk("agent_message_chunk", text)]
    }

    /// The final message: only the part the stream has not shown. The
    /// segment ends with it.
    fn final_text(&mut self, text: &str) -> Vec<Value> {
        let unseen = match text.strip_prefix(self.streamed.as_str()) {
            Some(rest) => rest.to_owned(),
            None => text.chars().skip(self.streamed.chars().count()).collect(),
        };
        let updates = if unseen.is_empty() {
            Vec::new()
        } else {
            vec![self.chunk("agent_message_chunk", &unseen)]
        };
        self.next_segment();
        updates
    }

    fn next_segment(&mut self) {
        self.segment += 1;
        self.streamed.clear();
    }

    fn tool_call(&mut self, payload: &Value) -> Vec<Value> {
        let id = payload["call_id"].as_str().unwrap_or_default().to_owned();
        let tool = payload["tool"].as_str().unwrap_or_default().to_owned();
        let arguments = &payload["arguments"];
        self.next_segment();
        self.open.insert(id.clone(), tool.clone());
        let mut call = json!({
            "sessionUpdate": "tool_call",
            "toolCallId": id,
            "title": title_of(&tool, arguments),
            "kind": kind_of(&tool),
            "status": "in_progress",
            "rawInput": arguments,
        });
        if let Some(path) = arguments.get("path").and_then(Value::as_str) {
            call["locations"] = json!([{"path": path}]);
        }
        let mut updates = vec![call];
        if tool == "todo" {
            updates.extend(plan(arguments.get("todos")));
        }
        updates
    }

    fn tool_result(&mut self, payload: &Value) -> Vec<Value> {
        let id = payload["call_id"].as_str().unwrap_or_default().to_owned();
        let tool = self.open.remove(&id).unwrap_or_default();
        let ok = payload["ok"].as_bool().unwrap_or(false);
        let text = payload["output"]
            .as_str()
            .map(str::to_owned)
            .or_else(|| payload["error"]["message"].as_str().map(str::to_owned))
            .unwrap_or_default();
        let mut updates = vec![json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": id,
            "status": if ok { "completed" } else { "failed" },
            "content": [{"type": "content", "content": {"type": "text", "text": text}}],
        })];
        if tool == "todo" && ok {
            let list: Option<Value> = serde_json::from_str(&text).ok();
            updates.extend(plan(list.as_ref().and_then(|l| l.get("todos"))));
        }
        updates
    }

    /// The end of a turn: every call it left open is failed.
    pub(crate) fn finish(&mut self) -> Vec<Value> {
        let open = std::mem::take(&mut self.open);
        self.next_segment();
        open.into_keys()
            .map(|id| {
                json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": id,
                    "status": "failed",
                    "content": [{"type": "content", "content": {"type": "text", "text": ENDED_WITHOUT_RESULT}}],
                })
            })
            .collect()
    }
}

/// A `plan` update of a todo list (`None` when there is none).
fn plan(todos: Option<&Value>) -> Option<Value> {
    let items = todos?.as_array()?;
    let entries: Vec<Value> = items
        .iter()
        .filter_map(|item| {
            let content = item.get("content")?.as_str()?;
            let status = match item.get("status").and_then(Value::as_str) {
                Some("completed" | "cancelled") => "completed",
                Some("in_progress") => "in_progress",
                _ => "pending",
            };
            Some(json!({"content": content, "priority": "medium", "status": status}))
        })
        .collect();
    Some(json!({"sessionUpdate": "plan", "entries": entries}))
}

/// The transcript replay of one stored message (user and assistant text).
pub(crate) fn replayed(role: &str, text: &str, index: usize) -> Option<Value> {
    let kind = match role {
        "user" => "user_message_chunk",
        "assistant" => "agent_message_chunk",
        _ => return None,
    };
    (!text.is_empty()).then(|| {
        json!({
            "sessionUpdate": kind,
            "messageId": format!("replay-{index}"),
            "content": {"type": "text", "text": text},
        })
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{ENDED_WITHOUT_RESULT, Projector, kind_of};

    fn event(kind: &str, payload: &serde_json::Value) -> serde_json::Value {
        json!({"type": kind, "payload": payload})
    }

    fn texts(updates: &[serde_json::Value]) -> Vec<String> {
        updates
            .iter()
            .map(|u| u["content"]["text"].as_str().unwrap_or_default().to_owned())
            .collect()
    }

    /// Scenario: Final text after streaming.
    #[test]
    fn only_the_unseen_remainder_of_the_final_text_is_sent() {
        let mut projector = Projector::default();
        let streamed = "a".repeat(200);
        let sent = projector.event(&event("assistant.delta", &json!({"text": streamed})));
        assert_eq!(texts(&sent)[0].len(), 200);
        let full = format!("{streamed}{}", "b".repeat(30));
        let rest = projector.event(&event("assistant.message", &json!({"text": full})));
        assert_eq!(texts(&rest), ["b".repeat(30)]);
        assert_eq!(rest[0]["messageId"], sent[0]["messageId"]);
    }

    #[test]
    fn a_tool_call_starts_a_new_message_segment() {
        let mut projector = Projector::default();
        let before = projector.event(&event("assistant.delta", &json!({"text": "Let me look."})));
        projector.event(&event(
            "tool.call",
            &json!({"call_id": "c1", "tool": "read_file", "arguments": {"path": "a.rs"}}),
        ));
        let after = projector.event(&event("assistant.delta", &json!({"text": "Done."})));
        assert_ne!(before[0]["messageId"], after[0]["messageId"]);
    }

    /// Scenario: Interrupted tool.
    #[test]
    fn calls_still_open_at_the_turn_end_are_failed() {
        let mut projector = Projector::default();
        let started = projector.event(&event(
            "tool.call",
            &json!({"call_id": "c1", "tool": "terminal", "arguments": {"command": "sleep 60"}}),
        ));
        assert_eq!(started[0]["kind"], "execute");
        projector.event(&event(
            "tool.call",
            &json!({"call_id": "c2", "tool": "read_file", "arguments": {"path": "x"}}),
        ));
        let done = projector.event(&event(
            "tool.result",
            &json!({"call_id": "c2", "tool": "read_file", "ok": false, "error": {"message": "no such file"}}),
        ));
        assert_eq!(
            done[0]["status"], "failed",
            "the executor's error flag is the verdict"
        );
        let ended = projector.finish();
        assert_eq!(ended.len(), 1);
        assert_eq!(ended[0]["toolCallId"], "c1");
        assert_eq!(ended[0]["status"], "failed");
        assert_eq!(
            ended[0]["content"][0]["content"]["text"],
            ENDED_WITHOUT_RESULT
        );
    }

    #[test]
    fn unmapped_tools_are_other_and_todos_become_a_plan() {
        assert_eq!(kind_of("mcp_db_query"), "other");
        let mut projector = Projector::default();
        let updates = projector.event(&event(
            "tool.call",
            &json!({"call_id": "t", "tool": "todo", "arguments": {"todos": [
                {"id": "1", "content": "write tests", "status": "in_progress"}
            ]}}),
        ));
        let plan = updates
            .iter()
            .find(|u| u["sessionUpdate"] == "plan")
            .unwrap();
        assert_eq!(plan["entries"][0]["content"], "write tests");
        assert_eq!(plan["entries"][0]["status"], "in_progress");
    }
}

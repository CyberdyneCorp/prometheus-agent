//! The `response` object and its stream (spec: api-server, Responses
//! endpoint and response store): the turn's output items in order, and
//! with `stream: true` the OpenAI Responses event types, each with a
//! monotonic `sequence_number`.

use pmt_core::Error;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::http::Sse;
use crate::responses::Head;
use crate::session;
use crate::turn::{Outcome, TurnEvent};

/// Builds a turn's output items and streams them.
pub(crate) struct Builder {
    id: String,
    model: String,
    head: Value,
    out: Option<mpsc::UnboundedSender<Sse>>,
    sequence: u64,
    items: Vec<Value>,
    /// The streamed message item still open: its index and text.
    open: Option<(usize, String)>,
    /// Item ids handed out.
    counter: u64,
}

impl Builder {
    pub(crate) fn new(head: &Head, out: Option<mpsc::UnboundedSender<Sse>>) -> Self {
        Self {
            id: head.id.clone(),
            model: head.model.clone(),
            head: json!({
                "previous_response_id": head.previous,
                "conversation": head.conversation.as_ref().map(|c| json!({"id": c})),
                "instructions": head.instructions,
                "store": head.store,
                "prometheus": session::stamp(&head.session, head.key.as_deref()),
            }),
            out,
            sequence: 0,
            items: Vec::new(),
            open: None,
            counter: 0,
        }
    }

    fn send(&mut self, kind: &str, mut data: Value) {
        let Some(out) = &self.out else {
            return;
        };
        data["type"] = kind.into();
        data["sequence_number"] = self.sequence.into();
        self.sequence += 1;
        let _ = out.send(Sse::named(kind, &data));
    }

    fn item_id(&mut self, prefix: &str) -> String {
        self.counter += 1;
        format!(
            "{prefix}_{}_{}",
            self.id.trim_start_matches("resp_"),
            self.counter
        )
    }

    /// The response object with `status` and the items so far.
    fn object(&self, status: &str) -> Value {
        let mut object = json!({
            "id": self.id,
            "object": "response",
            "created_at": jiff::Timestamp::now().as_second(),
            "status": status,
            "model": self.model,
            "output": self.items,
        });
        if let (Some(object), Some(head)) = (object.as_object_mut(), self.head.as_object()) {
            for (key, value) in head {
                object.insert(key.clone(), value.clone());
            }
        }
        object
    }

    /// `response.created` and `response.in_progress`.
    pub(crate) fn started(&mut self) {
        let object = self.object("in_progress");
        self.send("response.created", json!({"response": object}));
        self.send("response.in_progress", json!({"response": object}));
    }

    fn add(&mut self, item: Value) -> usize {
        let index = self.items.len();
        self.send(
            "response.output_item.added",
            json!({"output_index": index, "item": item}),
        );
        self.items.push(item);
        index
    }

    fn done(&mut self, index: usize) {
        let item = self.items[index].clone();
        self.send(
            "response.output_item.done",
            json!({"output_index": index, "item": item}),
        );
    }

    /// One turn event.
    pub(crate) fn event(&mut self, event: TurnEvent) {
        match event {
            TurnEvent::Delta(text) => self.delta(&text),
            TurnEvent::Reasoning(text) => self.reasoning(&text),
            TurnEvent::Commentary(text) => self.close("commentary", Some(&text)),
            TurnEvent::ToolStarted {
                call_id,
                tool,
                arguments,
            } => {
                self.close("commentary", None);
                let arguments = match arguments {
                    Value::String(s) => s,
                    other => other.to_string(),
                };
                let index = self.add(json!({"type": "function_call", "id": format!("fc_{call_id}"),
                    "call_id": call_id, "name": tool, "arguments": arguments, "status": "completed"}));
                self.done(index);
            }
            TurnEvent::ToolCompleted {
                call_id, output, ..
            } => {
                let index = self.add(
                    json!({"type": "function_call_output", "id": format!("fco_{call_id}"),
                    "call_id": call_id, "output": output, "status": "completed"}),
                );
                self.done(index);
            }
            _ => {}
        }
    }

    /// A reasoning item; it goes before the message its model call is
    /// still streaming (reasoning is reported once the call returns).
    fn reasoning(&mut self, text: &str) {
        let id = self.item_id("rs");
        let item = json!({"type": "reasoning", "id": id,
            "summary": [{"type": "summary_text", "text": text}]});
        let index = match self.open.as_mut() {
            Some((open, _)) => {
                let index = *open;
                *open += 1;
                self.items.insert(index, item.clone());
                self.send(
                    "response.output_item.added",
                    json!({"output_index": index, "item": item}),
                );
                index
            }
            None => self.add(item),
        };
        self.done(index);
    }

    fn message(&mut self, phase: &str, text: &str, status: &str) -> Value {
        let id = self.item_id("msg");
        json!({"type": "message", "id": id, "role": "assistant", "status": status, "phase": phase,
            "content": [{"type": "output_text", "text": text, "annotations": []}]})
    }

    fn delta(&mut self, text: &str) {
        if self.open.is_none() {
            let item = self.message("final_answer", "", "in_progress");
            let index = self.add(item.clone());
            self.send("response.content_part.added", json!({"item_id": item["id"], "output_index": index,
                "content_index": 0, "part": {"type": "output_text", "text": "", "annotations": []}}));
            self.open = Some((index, String::new()));
        }
        let Some((index, streamed)) = self.open.as_mut() else {
            return;
        };
        streamed.push_str(text);
        let index = *index;
        let item_id = self.items[index]["id"].clone();
        self.send(
            "response.output_text.delta",
            json!({"item_id": item_id, "output_index": index,
            "content_index": 0, "delta": text}),
        );
    }

    /// Ends the open message item as `phase` (with `text` when given),
    /// or adds a non-streamed commentary item.
    fn close(&mut self, phase: &str, text: Option<&str>) {
        match self.open.take() {
            Some((index, streamed)) => {
                let text = text.map_or(streamed, str::to_owned);
                self.items[index]["phase"] = phase.into();
                self.items[index]["status"] = "completed".into();
                self.items[index]["content"][0]["text"] = text.clone().into();
                let item_id = self.items[index]["id"].clone();
                self.send(
                    "response.output_text.done",
                    json!({"item_id": item_id, "output_index": index,
                    "content_index": 0, "text": text}),
                );
                self.done(index);
            }
            None => {
                if let Some(text) = text.filter(|t| !t.trim().is_empty()) {
                    let item = self.message(phase, text, "completed");
                    let index = self.add(item);
                    self.done(index);
                }
            }
        }
    }

    /// The finished response, after its terminal event.
    pub(crate) fn finished(&mut self, outcome: &Outcome) -> Value {
        if let Some(text) = &outcome.final_response {
            if self.open.is_some() {
                self.close("final_answer", Some(text));
            } else {
                let item = self.message("final_answer", text, "completed");
                let index = self.add(item);
                self.done(index);
            }
        }
        let status = outcome.status();
        let mut response = self.object(status);
        let usage = outcome.openai_usage();
        response["usage"] = json!({
            "input_tokens": usage["prompt_tokens"],
            "output_tokens": usage["completion_tokens"],
            "total_tokens": usage["total_tokens"],
            "input_tokens_details": {"cached_tokens": usage["cache_read_tokens"]},
            "output_tokens_details": {"reasoning_tokens": usage["completion_tokens_details"]["reasoning_tokens"]},
            "cache_read_tokens": usage["cache_read_tokens"],
            "cache_write_tokens": usage["cache_write_tokens"],
        });
        if status == "incomplete" {
            response["incomplete_details"] = json!({"reason": "iteration_budget_exhausted"});
        }
        if status == "failed" {
            let code = outcome
                .error
                .as_ref()
                .map_or(outcome.exit_reason.as_str(), |e| e.code.as_str());
            response["error"] = json!({"code": code, "message": outcome.error_text()});
        }
        if let Some(runtime) = &outcome.runtime {
            response["prometheus"]["runtime"] =
                json!({"provider": runtime.provider, "model": runtime.model});
        }
        let kind = match status {
            "completed" => "response.completed",
            "incomplete" => "response.incomplete",
            "cancelled" => "response.cancelled",
            _ => "response.failed",
        };
        self.send(kind, json!({"response": response}));
        self.out = None;
        response
    }

    /// The turn was never admitted.
    pub(crate) fn refused(&mut self, err: &Error) {
        let mut response = self.object("failed");
        response["error"] = json!({"code": err.code().code, "message": err.message()});
        self.send("response.failed", json!({"response": response}));
        self.out = None;
    }
}

/// `response` as stored: each tool output capped to `max` characters
/// (`0`: verbatim) as its head plus `...[<n> more chars]`.
pub(crate) fn for_storage(response: &Value, max: u64) -> Value {
    let mut stored = response.clone();
    let Ok(max) = usize::try_from(max) else {
        return stored;
    };
    if max == 0 {
        return stored;
    }
    if let Some(items) = stored["output"].as_array_mut() {
        for item in items
            .iter_mut()
            .filter(|i| i["type"] == "function_call_output")
        {
            let output = item["output"].as_str().unwrap_or_default();
            let total = output.chars().count();
            if total > max {
                let head: String = output.chars().take(max).collect();
                item["output"] = format!("{head}...[{} more chars]", total - max).into();
            }
        }
    }
    stored
}

/// Streams a stored response again, item by item.
pub(crate) fn replay_stream(response: &Value, out: &mpsc::UnboundedSender<Sse>) {
    let mut sequence = 0u64;
    let mut send = |kind: &str, mut data: Value| {
        data["type"] = kind.into();
        data["sequence_number"] = sequence.into();
        sequence += 1;
        let _ = out.send(Sse::named(kind, &data));
    };
    let mut started = response.clone();
    started["status"] = "in_progress".into();
    started["output"] = json!([]);
    send("response.created", json!({"response": started}));
    for (index, item) in response["output"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        send(
            "response.output_item.added",
            json!({"output_index": index, "item": item}),
        );
        if item["type"] == "message" {
            let text = item["content"][0]["text"].clone();
            send(
                "response.output_text.delta",
                json!({"item_id": item["id"], "output_index": index,
                "content_index": 0, "delta": text}),
            );
        }
        send(
            "response.output_item.done",
            json!({"output_index": index, "item": item}),
        );
    }
    let kind = match response["status"].as_str() {
        Some("completed") => "response.completed",
        Some("incomplete") => "response.incomplete",
        Some("cancelled") => "response.cancelled",
        _ => "response.failed",
    };
    send(kind, json!({"response": response}));
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{Builder, for_storage};
    use crate::responses::Head;
    use crate::turn::{Item, Outcome, TurnEvent};

    fn head() -> Head {
        Head {
            id: "resp_1".into(),
            model: "prometheus-agent".into(),
            session: "s".into(),
            key: None,
            previous: None,
            conversation: None,
            instructions: None,
            store: true,
            max_stored: 100,
            tool_output_max: 0,
        }
    }

    #[test]
    fn output_lists_the_turns_items_in_order() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut builder = Builder::new(&head(), Some(tx));
        builder.started();
        builder.event(TurnEvent::Reasoning("plan".into()));
        builder.event(TurnEvent::Delta("Let me look.".into()));
        builder.event(TurnEvent::Commentary("Let me look.".into()));
        builder.event(TurnEvent::ToolStarted {
            call_id: "c1".into(),
            tool: "terminal".into(),
            arguments: json!({"command": "ls"}),
        });
        builder.event(TurnEvent::ToolCompleted {
            call_id: "c1".into(),
            tool: "terminal".into(),
            ok: true,
            duration_ms: Some(3),
            output: "README.md".into(),
        });
        builder.event(TurnEvent::Delta("Here.".into()));
        let outcome = Outcome {
            exit_reason: "completed".into(),
            final_response: Some("Here.".into()),
            items: vec![Item::Reasoning("plan".into())],
            ..Outcome::default()
        };
        let response = builder.finished(&outcome);
        let kinds: Vec<_> = response["output"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| {
                format!(
                    "{}:{}",
                    i["type"].as_str().unwrap(),
                    i["phase"].as_str().unwrap_or("-")
                )
            })
            .collect();
        assert_eq!(
            kinds,
            [
                "reasoning:-",
                "message:commentary",
                "function_call:-",
                "function_call_output:-",
                "message:final_answer"
            ]
        );
        assert_eq!(response["status"], "completed");
        let mut last = None;
        while let Ok(event) = rx.try_recv() {
            let data: serde_json::Value = serde_json::from_str(&event.data).unwrap();
            let n = data["sequence_number"].as_u64().unwrap();
            assert!(last.is_none_or(|l| n == l + 1));
            last = Some(n);
        }
        assert!(last.unwrap() > 5);
    }

    #[test]
    fn stored_tool_outputs_are_capped() {
        let response =
            json!({"output": [{"type": "function_call_output", "output": "abcdefghij"}]});
        let stored = for_storage(&response, 4);
        assert_eq!(stored["output"][0]["output"], "abcd...[6 more chars]");
        assert_eq!(for_storage(&response, 0), response);
    }
}

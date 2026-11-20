//! What the `NeMo` Relay host is fed (spec: observability, `NeMo` Relay and
//! Langfuse integrations): the session, turn, model-call and tool
//! lifecycle of every session, read from its trajectory events, as one
//! JSON operation each. The binary installs a [`Feed`] (the Relay host in
//! the python bridge) when `observability.nemo_relay.enabled`; every
//! session trace opened afterwards translates its events with [`ops`].
//!
//! Payloads (message text, tool arguments and output) are present only
//! when the feed captures content (`observability.otel.capture_content`),
//! and then only redacted.

use std::sync::{Arc, OnceLock, RwLock};

use serde_json::{Value, json};

use crate::otel::Identity;
use crate::trajectory::events::Event;
use crate::trajectory::writer::Emit;

/// Where the operations go (the Relay host).
pub trait Feed: Send + Sync + std::fmt::Debug {
    /// Takes one operation; never blocks the session (a full queue drops).
    fn send(&self, op: Value);
    /// Whether payloads are sent (`observability.otel.capture_content`).
    fn captures_content(&self) -> bool;
}

fn slot() -> &'static RwLock<Option<Arc<dyn Feed>>> {
    static SLOT: OnceLock<RwLock<Option<Arc<dyn Feed>>>> = OnceLock::new();
    SLOT.get_or_init(RwLock::default)
}

/// The installed feed, when Relay is on.
#[must_use]
pub fn installed() -> Option<Arc<dyn Feed>> {
    slot()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// Installs `feed` for every session opened from now on (`None`
/// uninstalls).
pub fn install(feed: Option<Arc<dyn Feed>>) {
    *slot()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = feed;
}

/// Redacts a payload before it leaves the process.
pub type Clean<'a> = &'a dyn Fn(&str) -> String;

/// The operations `emit` makes, `first` when it is the session's first
/// event (the session scope opens before it). `clean` is given only when
/// payloads are captured.
#[must_use]
pub fn ops(identity: &Identity, first: bool, emit: &Emit, clean: Option<Clean<'_>>) -> Vec<Value> {
    let session = identity.session_id.as_str();
    let mut ops = Vec::new();
    if first {
        ops.push(json!({
            "op": "session_start",
            "session": session,
            "metadata": {
                "profile": identity.profile,
                "source": identity.source,
                "root_session_id": identity.root_session_id,
            },
        }));
    }
    let turn = emit.turn_id.as_deref().unwrap_or_default();
    let op = match &emit.event {
        Event::SessionEnd(end) => Some(json!({
            "op": "session_end", "session": session, "metadata": {"reason": end.reason},
        })),
        Event::TurnStart(_) => Some(json!({"op": "turn_start", "session": session, "turn": turn})),
        Event::TurnEnd(end) => Some(json!({
            "op": "turn_end", "session": session, "turn": turn,
            "metadata": {"reason": end.reason, "iterations": end.iterations, "usage": end.usage},
        })),
        Event::ModelCall(call) => Some(json!({
            "op": "llm", "session": session, "turn": turn, "name": call.model,
            "metadata": {
                "provider": call.provider, "model": call.model, "purpose": call.purpose,
                "latency_ms": call.latency_ms, "tokens": call.tokens, "cost_usd": call.cost_usd,
                "cache_hit": call.cache_hit,
            },
        })),
        Event::ToolCall(call) => {
            let mut op = json!({
                "op": "tool_start", "session": session, "turn": turn,
                "call": call.call_id, "name": call.tool,
            });
            if let Some(clean) = clean {
                op["input"] = Value::String(clean(&call.arguments.to_string()));
            }
            Some(op)
        }
        Event::ToolResult(result) => {
            let mut op = json!({
                "op": "tool_end", "session": session, "call": result.call_id,
                "metadata": {"ok": result.ok, "duration_ms": result.duration_ms},
            });
            if let Some(clean) = clean {
                let text = result
                    .output
                    .clone()
                    .or_else(|| result.error.as_ref().map(|e| e.message.clone()))
                    .unwrap_or_default();
                op["output"] = Value::String(clean(&text));
            }
            Some(op)
        }
        Event::UserMessage(m) => clean.map(|clean| message(session, turn, "user", &clean(&m.text))),
        Event::AssistantMessage(m) => {
            clean.map(|clean| message(session, turn, "assistant", &clean(&m.text)))
        }
        _ => None,
    };
    ops.extend(op);
    ops
}

fn message(session: &str, turn: &str, role: &str, text: &str) -> Value {
    json!({"op": "message", "session": session, "turn": turn, "role": role, "text": text})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trajectory::payload::{MessageText, SessionEnd, ToolCall, TurnStart};

    fn identity() -> Identity {
        Identity {
            profile: "default".into(),
            session_id: "s1".into(),
            root_session_id: "s1".into(),
            source: "cli".into(),
        }
    }

    fn in_turn(event: Event) -> Emit {
        let mut emit = Emit::now(event);
        emit.turn_id = Some("t1".into());
        emit
    }

    #[test]
    fn the_first_event_opens_the_session_and_turns_nest_in_it() {
        let ops = ops(
            &identity(),
            true,
            &in_turn(Event::TurnStart(TurnStart { message_id: None })),
            None,
        );
        let names: Vec<&str> = ops.iter().map(|o| o["op"].as_str().unwrap()).collect();
        assert_eq!(names, ["session_start", "turn_start"]);
        assert_eq!(ops[1]["turn"], "t1");
        let end = super::ops(
            &identity(),
            false,
            &Emit::now(Event::SessionEnd(SessionEnd {
                reason: "user_exit".into(),
            })),
            None,
        );
        assert_eq!(end[0]["op"], "session_end");
    }

    #[test]
    fn payloads_are_sent_only_when_captured_and_then_redacted() {
        let call = in_turn(Event::ToolCall(ToolCall {
            call_id: "c1".into(),
            tool: "read_file".into(),
            arguments: json!({"path": "secret.txt"}),
        }));
        let bare = ops(&identity(), false, &call, None);
        assert!(bare[0].get("input").is_none(), "{bare:?}");
        let said = in_turn(Event::UserMessage(MessageText {
            message_id: None,
            text: "hello".into(),
            author: None,
        }));
        assert!(ops(&identity(), false, &said, None).is_empty());
        let redact = |text: &str| text.replace("secret", "[REDACTED]");
        let captured = ops(&identity(), false, &call, Some(&redact));
        assert_eq!(captured[0]["input"], r#"{"path":"[REDACTED].txt"}"#);
        let said = ops(&identity(), false, &said, Some(&redact));
        assert_eq!(said[0]["role"], "user");
        assert_eq!(said[0]["text"], "hello");
    }
}

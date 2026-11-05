//! Lifecycle hooks of a session that is not a gateway session (spec:
//! plugin-system, Typed hook registry; Streaming and lifecycle observer
//! hooks): `on_turn_start`, `post_llm_call` after every loop iteration,
//! `post_compaction` after a committed compaction and `on_turn_end` (with
//! the first 500 characters of the reply), fed from the session's own
//! trajectory events with the payloads the gateway gives its turns
//! (`pmt-gateway` lane `hook_events`). Gateway sessions are left out: the
//! gateway fires these hooks for its turns itself.

use std::sync::{Arc, Mutex, PoisonError};

use pmt_agent::turn::EventSink;
use pmt_agent::turn::model::BoxFuture;
use pmt_obs::trajectory::events::Event;
use pmt_obs::trajectory::writer::Emit;
use pmt_plugins::host::CallInfo;
use serde_json::{Map, Value, json};

use crate::plugins_host::ProfilePlugins;

/// How much of the reply `on_turn_end` carries.
const RESPONSE_CHARS: usize = 500;

/// Forwards every event to `inner`, then fires the hook it maps onto.
pub(super) struct LifecycleHooks {
    inner: Arc<dyn EventSink>,
    plugins: ProfilePlugins,
    call: CallInfo,
    /// The turn's latest assistant text (its reply once the turn ends).
    reply: Mutex<Option<String>>,
}

impl LifecycleHooks {
    pub(super) fn new(inner: Arc<dyn EventSink>, plugins: ProfilePlugins, call: CallInfo) -> Self {
        Self {
            inner,
            plugins,
            call,
            reply: Mutex::new(None),
        }
    }

    fn session(&self) -> String {
        self.call.session_id.clone().unwrap_or_default()
    }

    /// The hook `event` maps onto and its payload.
    fn hook_of(&self, event: &Event) -> Option<(&'static str, Value)> {
        let session = self.session();
        let mut reply = self.reply.lock().unwrap_or_else(PoisonError::into_inner);
        Some(match event {
            Event::TurnStart(_) => {
                *reply = None;
                ("on_turn_start", json!({"session_id": session}))
            }
            Event::AssistantMessage(message) => {
                *reply = Some(message.text.clone());
                return None;
            }
            Event::TurnStep(step) => (
                "post_llm_call",
                json!({
                    "session_id": session,
                    "iteration": step.iteration,
                    "tool_names": step.calls.iter().map(|c| c.tool.as_str()).collect::<Vec<_>>(),
                }),
            ),
            Event::CompressionCommitted(committed) => (
                "post_compaction",
                json!({
                    "session_id": session,
                    "old_session_id": session,
                    "in_place": true,
                    "compression_count": committed.epoch,
                    "trigger": committed.trigger,
                    "tokens_before": committed.tokens_before,
                    "tokens_after": committed.tokens_after,
                }),
            ),
            Event::TurnEnd(end) => {
                let response: Option<String> = reply
                    .take()
                    .map(|text| text.chars().take(RESPONSE_CHARS).collect());
                (
                    "on_turn_end",
                    json!({
                        "session_id": session,
                        "exit_reason": end.reason,
                        "response": response,
                    }),
                )
            }
            _ => return None,
        })
    }
}

impl EventSink for LifecycleHooks {
    fn emit(&self, emit: Emit) -> BoxFuture<'_, Result<(), pmt_core::Error>> {
        Box::pin(async move {
            let hook = self.hook_of(&emit.event);
            let forwarded = self.inner.emit(emit).await;
            if let Some((name, Value::Object(fields))) = hook {
                self.plugins.fire_observer(&self.call, name, fields).await;
            }
            forwarded
        })
    }
}

/// `on_session_start` for a session opened in this process.
pub(super) fn session_start(session: &str, platform: &str) -> Map<String, Value> {
    let mut fields = Map::new();
    fields.insert("session_id".to_owned(), json!(session));
    fields.insert("platform".to_owned(), json!(platform));
    fields.insert("reset_reason".to_owned(), Value::Null);
    fields
}

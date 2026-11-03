//! Lifecycle hooks a gateway turn feeds from its session's trajectory
//! (spec: plugin-system, Gateway hook directories; messaging-gateway,
//! Gateway lifecycle hooks are typed): `post_llm_call` after every loop
//! iteration (`turn.step`, Hermes `agent:step`) and `post_compaction` after a
//! committed compaction (`compression.committed`, Hermes
//! `session:compress`), plus the `on_turn_end` payload with the reply.

use pmt_rpc::contracts::TurnFinished;
use serde_json::{Value, json};

use super::Lane;

/// How much of the reply `on_turn_end` carries (Hermes `agent:end`).
const RESPONSE_CHARS: usize = 500;

impl Lane {
    /// Emits the hook a trajectory event of the session maps onto.
    pub(super) async fn hook_event(&self, kind: &str, payload: &Value) {
        let session = self.session.clone().unwrap_or_default();
        let (name, fields) = match kind {
            "turn.step" => ("post_llm_call", step(&session, payload)),
            "compression.committed" => ("post_compaction", compaction(&session, payload)),
            _ => return,
        };
        self.shared
            .options
            .hooks
            .emit(&self.profile, name, fields)
            .await;
    }
}

/// `post_llm_call`: the iteration and the tools it called.
fn step(session: &str, payload: &Value) -> Value {
    let tool_names: Vec<&str> = payload["calls"]
        .as_array()
        .map(|calls| calls.iter().filter_map(|c| c["tool"].as_str()).collect())
        .unwrap_or_default();
    json!({
        "session_id": session,
        "iteration": payload["iteration"],
        "tool_names": tool_names,
    })
}

/// `post_compaction`: compaction rewrites the session in place (the id is
/// kept), and its epoch counts the compactions so far.
fn compaction(session: &str, payload: &Value) -> Value {
    json!({
        "session_id": session,
        "old_session_id": session,
        "in_place": true,
        "compression_count": payload["epoch"],
        "trigger": payload["trigger"],
        "tokens_before": payload["tokens_before"],
        "tokens_after": payload["tokens_after"],
    })
}

/// `on_turn_end`: how the turn ended and the first 500 characters of its
/// reply.
pub(super) fn turn_end(session: &str, finished: &TurnFinished) -> Value {
    let response: Option<String> = finished
        .final_response
        .as_deref()
        .map(|text| text.chars().take(RESPONSE_CHARS).collect());
    json!({
        "session_id": session,
        "exit_reason": finished.exit_reason,
        "response": response,
    })
}

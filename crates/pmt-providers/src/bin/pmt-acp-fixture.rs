//! A fake agent CLI speaking the Agent Client Protocol on stdio, for the
//! `external_process` provider tests (built with the crate, never shipped
//! with Prometheus): it answers `initialize` and `session/new`, asks one
//! permission, then streams a thought naming the permission's outcome and
//! the message `agent saw: <last line of the prompt>` before ending the
//! turn.

use std::io::{BufRead as _, Write};

use serde_json::{Value, json};

fn send(out: &mut impl Write, frame: &Value) {
    writeln!(out, "{frame}").expect("stdout");
    out.flush().expect("stdout");
}

fn update(out: &mut impl Write, kind: &str, text: &str) {
    send(
        out,
        &json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "fake-1",
            "update": {"sessionUpdate": kind, "content": {"type": "text", "text": text}}}}),
    );
}

fn main() {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    let mut lines = stdin.lock().lines();
    while let Some(Ok(line)) = lines.next() {
        let Ok(frame) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let id = frame["id"].clone();
        match frame["method"].as_str() {
            Some("initialize") => send(
                &mut out,
                &json!({"jsonrpc": "2.0", "id": id, "result": {"protocolVersion": 1, "agentCapabilities": {}}}),
            ),
            Some("session/new") => send(
                &mut out,
                &json!({"jsonrpc": "2.0", "id": id, "result": {"sessionId": "fake-1"}}),
            ),
            Some("session/prompt") => {
                let prompt = frame["params"]["prompt"][0]["text"].as_str().unwrap_or("");
                let last = prompt.trim().lines().last().unwrap_or("").to_owned();
                send(
                    &mut out,
                    &json!({"jsonrpc": "2.0", "id": 99, "method": "session/request_permission",
                        "params": {"sessionId": "fake-1"}}),
                );
                let answer = lines
                    .next()
                    .and_then(Result::ok)
                    .and_then(|l| serde_json::from_str::<Value>(&l).ok())
                    .unwrap_or_default();
                let outcome = answer["result"]["outcome"]["outcome"]
                    .as_str()
                    .unwrap_or("?");
                update(
                    &mut out,
                    "agent_thought_chunk",
                    &format!("permission {outcome}"),
                );
                update(
                    &mut out,
                    "agent_message_chunk",
                    &format!("agent saw: {last}"),
                );
                send(
                    &mut out,
                    &json!({"jsonrpc": "2.0", "id": id, "result": {"stopReason": "end_turn"}}),
                );
            }
            _ => {}
        }
    }
}

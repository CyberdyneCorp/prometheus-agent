//! The Raft wake-only adapter (spec: platform-adapters, Raft wake-only
//! adapter). Its sidecar (`sidecar/raft-wake.mjs`) runs the Raft CLI
//! bridge and queues the wake hints it posts; each poll turns them into a
//! content-free wake notice in chat `raft`, so the agent reads and
//! answers with the Raft CLI. A hint carrying a content field (`text`,
//! `body`, `content`, `messages`) is refused by the sidecar and, should
//! one get through, dropped here. The adapter holds no Raft credentials
//! or cursors, and a reply is not delivered (the agent uses the CLI).

use std::sync::Mutex;

use adapter_kit::message::{self, Inbound};
use adapter_kit::{Guest, HttpRequest, HttpResponse, host, net, respond};
use serde_json::{Value, json};

/// What the agent is told; the wake's id is the only detail it carries.
const WAKE_NOTICE: &str = "Raft wake hint received: new Raft messages may be pending. Read and answer them with the Raft CLI (run `raft manual get raft-cli-overview` first if you have not this session).";
const CHAT: &str = "raft";
const CONTENT_FIELDS: [&str; 4] = ["text", "body", "content", "messages"];
const ID_FIELDS: [&str; 6] = [
    "eventId",
    "attemptId",
    "messageId",
    "delivery_id",
    "wake_id",
    "id",
];

struct Sidecar {
    url: String,
    token: String,
    seq: u64,
}

static SIDECAR: Mutex<Option<Sidecar>> = Mutex::new(None);

fn carries_content(value: &Value) -> bool {
    match value {
        Value::Object(map) => map
            .iter()
            .any(|(k, v)| CONTENT_FIELDS.contains(&k.as_str()) || carries_content(v)),
        Value::Array(items) => items.iter().any(carries_content),
        _ => false,
    }
}

/// The hint's own id, when it is a short safe scalar.
fn wake_id(hint: &Value) -> Option<String> {
    ID_FIELDS.iter().find_map(|key| {
        let id = hint[*key].as_str()?;
        let safe = !id.is_empty()
            && id.len() <= 120
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "._:@/ -".contains(c));
        safe.then(|| id.to_owned())
    })
}

impl Sidecar {
    fn call(&self, path: &str, body: &Value) -> Result<Value, String> {
        let auth = format!("Bearer {}", self.token);
        net::json(
            "POST",
            &format!("{}{path}", self.url),
            &[("authorization", &auth)],
            body,
        )
    }
}

/// The notice for a wake `id`.
fn notice(id: &str) -> String {
    format!("{WAKE_NOTICE} (wake {id})")
}

struct Adapter;

impl Guest for Adapter {
    fn descriptor() -> String {
        json!({
            "platform": "raft",
            "max_message_length": 4096,
            "length_unit": "chars",
            "splits_long_messages": false,
            "supports_edit": false,
            "supports_delete": false,
            "supports_typing": false,
            "supports_reactions": false,
            "supports_threads": false,
            "interactive": "none",
            "media_in": [],
            "media_out": [],
            "delivery_receipts": false,
            "read_receipts": false,
            "streaming_tts": false,
            "markup": "plain",
            "binds_http": false,
            "enforces_own_access_policy": "allowlist",
        })
        .to_string()
    }

    fn start(options: String) -> Result<(), String> {
        let options: Value = serde_json::from_str(&options).map_err(|e| e.to_string())?;
        let profile = options["raft_profile"]
            .as_str()
            .filter(|p| !p.is_empty())
            .ok_or("options.raft_profile is required")?;
        let url = options["sidecar"]["url"]
            .as_str()
            .ok_or("the Raft wake sidecar is not running (options.sidecar missing)")?;
        let sidecar = Sidecar {
            url: url.trim_end_matches('/').to_owned(),
            token: options["sidecar"]["token"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            seq: host::load("seq").and_then(|v| v.as_u64()).unwrap_or(0),
        };
        let command = options["raft_command"].as_str().unwrap_or("raft");
        sidecar.call("/start", &json!({"profile": profile, "command": command}))?;
        *SIDECAR.lock().map_err(|e| e.to_string())? = Some(sidecar);
        Ok(())
    }

    fn stop() {
        if let Ok(mut slot) = SIDECAR.lock() {
            slot.take();
        }
    }

    fn poll() -> Result<String, String> {
        let mut slot = SIDECAR.lock().map_err(|e| e.to_string())?;
        let sidecar = slot.as_mut().ok_or("not started")?;
        let answer = sidecar.call("/wakes", &json!({}))?;
        let mut inbound = Vec::new();
        for hint in answer["wakes"].as_array().into_iter().flatten() {
            if carries_content(hint) {
                host::warn("raft: dropped a wake hint that carried message content");
                continue;
            }
            let id = wake_id(hint).unwrap_or_else(|| {
                sidecar.seq += 1;
                host::store("seq", &json!(sidecar.seq));
                format!("wake-{}", sidecar.seq)
            });
            inbound.push(Inbound::text(&id, CHAT, "dm", CHAT, &notice(&id)));
        }
        Ok(message::batch(&inbound))
    }

    /// The agent answers with the Raft CLI; nothing is delivered here.
    fn send(_message: String) -> Result<String, String> {
        Ok("raft-cli".to_owned())
    }

    fn chat_info(_chat: String) -> Result<String, String> {
        Ok(message::chat_info("Raft", "dm"))
    }

    fn handle_http(_request: HttpRequest) -> HttpResponse {
        respond(404, "text/plain", "")
    }

    /// Wakes come only from the bridge the sidecar started (its per-start
    /// token is checked there).
    fn authorize_sender(inbound: String) -> String {
        let inbound: Value = serde_json::from_str(&inbound).unwrap_or(Value::Null);
        if inbound["sender"] == CHAT {
            "allow"
        } else {
            "deny"
        }
        .to_owned()
    }
}

adapter_kit::bindings::export_adapter!(Adapter);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_is_found_at_any_depth_and_ids_must_be_safe() {
        assert!(carries_content(&json!({"meta": [{"body": "x"}]})));
        assert!(!carries_content(&json!({"eventId": "e1", "channel": "c"})));
        assert_eq!(wake_id(&json!({"eventId": "e-1"})).as_deref(), Some("e-1"));
        assert_eq!(wake_id(&json!({"eventId": "<script>"})), None);
    }
}

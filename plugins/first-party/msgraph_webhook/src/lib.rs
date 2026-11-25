//! The Microsoft Graph change-notification listener (spec:
//! platform-adapters, Microsoft Graph change-notification listener) as an
//! adapter: [`adapter_kit::graph::Listener`] answers Graph on the shared
//! listener, and every notification it admits becomes a message from
//! `msgraph` in the chat `msgraph:<subscription id>` (the agent works on
//! it like any message, with no human waiting). Its text is the option
//! `prompt` rendered with `{resource}`, `{change_type}`,
//! `{subscription_id}` and `{notification.<dotted.field>}` (unknown
//! placeholders stay as written), or the notification as JSON. Replies
//! have nobody to go to: they are appended to the plugin's
//! `replies.jsonl` (its last 500) and logged.

use std::sync::{Mutex, MutexGuard};

use adapter_kit::graph::{self, Listener};
use adapter_kit::message::{self, Inbound, Outgoing};
use adapter_kit::{Guest, HttpRequest, HttpResponse, clock, host, respond};
use serde_json::{Value, json};

/// The sender every notification comes from.
const SENDER: &str = "msgraph";
const REPLIES: &str = "replies.jsonl";
const KEEP_REPLIES: usize = 500;
/// The longest notification JSON put in a default prompt.
const JSON_CHARS: usize = 4000;

struct State {
    listener: Listener,
    prompt: Option<String>,
    inbox: Vec<Inbound>,
    sent: u64,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

fn lock() -> Result<MutexGuard<'static, Option<State>>, String> {
    STATE.lock().map_err(|e| e.to_string())
}

/// The value at `path` (`a.b.c`) of `value`, as text.
fn field(value: &Value, path: &str) -> Option<String> {
    let found = path.split('.').try_fold(value, |v, part| v.get(part))?;
    Some(match found {
        Value::String(s) => s.clone(),
        other => other.to_string().chars().take(2000).collect(),
    })
}

/// `template` with its placeholders filled from `notification`.
fn render(template: &str, notification: &Value) -> String {
    let mut out = String::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            out.push_str(&rest[open..]);
            return out;
        };
        let key = &after[..close];
        let value = match key {
            "resource" => field(notification, "resource"),
            "change_type" => field(notification, "changeType"),
            "subscription_id" => field(notification, "subscriptionId"),
            _ => key
                .strip_prefix("notification.")
                .and_then(|path| field(notification, path)),
        };
        match value {
            Some(v) => out.push_str(&v),
            None => out.push_str(&rest[open..open + close + 2]),
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    out
}

fn prompt(template: Option<&str>, notification: &Value) -> String {
    match template {
        Some(t) => render(t, notification),
        None => {
            let json: String = serde_json::to_string_pretty(notification)
                .unwrap_or_default()
                .chars()
                .take(JSON_CHARS)
                .collect();
            format!("Microsoft Graph change notification:\n\n```json\n{json}\n```")
        }
    }
}

fn chat_of(notification: &Value) -> String {
    format!(
        "msgraph:{}",
        notification["subscriptionId"].as_str().unwrap_or("unknown")
    )
}

struct Adapter;

impl Guest for Adapter {
    fn descriptor() -> String {
        json!({
            "platform": "msgraph_webhook",
            "max_message_length": 4000,
            "length_unit": "chars",
            "splits_long_messages": true,
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
            "binds_http": true,
            "max_body_bytes": 1_048_576,
            "enforces_own_access_policy": "allowlist",
        })
        .to_string()
    }

    fn start(options: String) -> Result<(), String> {
        let options: Value = serde_json::from_str(&options).map_err(|e| e.to_string())?;
        let listener = Listener::from_options(&options)?;
        *lock()? = Some(State {
            listener,
            prompt: options["prompt"]
                .as_str()
                .filter(|p| !p.is_empty())
                .map(str::to_owned),
            inbox: Vec::new(),
            sent: 0,
        });
        Ok(())
    }

    fn stop() {
        if let Ok(mut state) = lock() {
            *state = None;
        }
    }

    fn poll() -> Result<String, String> {
        clock::tick();
        let mut guard = lock()?;
        let state = guard.as_mut().ok_or("not started")?;
        Ok(message::batch(&std::mem::take(&mut state.inbox)))
    }

    fn send(message: String) -> Result<String, String> {
        let out = Outgoing::parse(&message)?;
        if !out.op.is_empty() {
            return Err("unsupported".to_owned());
        }
        let mut guard = lock()?;
        let state = guard.as_mut().ok_or("not started")?;
        state.sent += 1;
        let id = format!("reply-{}", state.sent);
        let line = json!({"id": id, "chat": out.chat, "text": out.text}).to_string();
        host::append_line(REPLIES, &line, KEEP_REPLIES);
        host::info(&format!(
            "reply for {}: {}",
            out.chat,
            out.text.chars().take(200).collect::<String>()
        ));
        Ok(id)
    }

    fn chat_info(chat: String) -> Result<String, String> {
        Ok(message::chat_info(&chat, "dm"))
    }

    fn handle_http(request: HttpRequest) -> HttpResponse {
        let Ok(mut guard) = lock() else {
            return respond(503, "text/plain", "busy");
        };
        let Some(state) = guard.as_mut() else {
            return respond(503, "text/plain", "not started");
        };
        let (answer, admitted) = state.listener.handle(&request);
        for notification in admitted {
            let mut inbound = Inbound::text(
                &graph::receipt(&notification),
                &chat_of(&notification),
                "dm",
                SENDER,
                &prompt(state.prompt.as_deref(), &notification),
            );
            inbound.sender_name = Some("Microsoft Graph".to_owned());
            state.inbox.push(inbound);
        }
        answer
    }

    fn authorize_sender(inbound: String) -> String {
        // Only admitted notifications (clientState checked) come from it.
        let inbound: Value = serde_json::from_str(&inbound).unwrap_or_default();
        if inbound["sender"] == SENDER {
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
    fn prompts_fill_known_placeholders_only() {
        let n = json!({"resource": "users/u1/messages/m1", "changeType": "created", "subscriptionId": "s1", "resourceData": {"id": "m1"}});
        assert_eq!(
            render(
                "{change_type} {resource} ({notification.resourceData.id}) {other}",
                &n
            ),
            "created users/u1/messages/m1 (m1) {other}"
        );
        assert!(prompt(None, &n).starts_with("Microsoft Graph change notification:"));
        assert_eq!(chat_of(&n), "msgraph:s1");
    }
}

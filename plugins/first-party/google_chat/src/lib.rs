//! The Google Chat adapter (spec: platform-adapters, Google Chat adapter).
//! Events arrive by Pub/Sub pull (`options.project_id`,
//! `options.subscription`, `max_messages` per pull, default 1) or, with
//! `options.mode = "http"`, as callbacks on the shared listener, each
//! admitted only when its Google-signed ID token names the configured
//! `audience` and `service_account_email` (401 otherwise). [`inbound`]
//! reads events, [`outbound`] answers through the Chat API as the app,
//! [`setup`] runs `/setup-files`, [`auth`] holds the credentials.

mod auth;
mod inbound;
mod outbound;
mod setup;

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use adapter_kit::message::{Inbound, Outgoing};
use adapter_kit::{Guest, HttpRequest, HttpResponse, clock, host, respond};
use serde_json::{Value, json};

/// How often (in polls) the subscription is pulled.
const PULL_POLLS: u64 = 4;
const THINKING: &str = "Prometheus is thinking…";

#[derive(Debug, Clone, PartialEq, Eq)]
enum Mode {
    /// Pull `projects/<project>/subscriptions/<subscription>`.
    PubSub { subscription: String, max: u64 },
    /// Callbacks on the shared listener.
    Http,
}

#[derive(Debug, Clone)]
struct Settings {
    mode: Mode,
    typing_indicator: bool,
    oauth_client_id: Option<String>,
    redirect_uri: String,
}

/// The started adapter.
struct GoogleChat {
    settings: Settings,
    auth: auth::Auth,
    inbox: Vec<Inbound>,
    /// Secret writes for the host (`store_secret` poll items).
    writes: Vec<Value>,
    /// The latest human sender of each space (whose token sends files).
    senders: HashMap<String, String>,
    /// The working-state message of each `space|thread`, until the reply
    /// replaces it.
    thinking: HashMap<String, String>,
    /// Card message names (an edit replaces their card).
    cards: Vec<String>,
    polls: u64,
    clicks: u64,
}

static CHAT: Mutex<Option<GoogleChat>> = Mutex::new(None);

fn lock() -> Result<MutexGuard<'static, Option<GoogleChat>>, String> {
    CHAT.lock().map_err(|e| e.to_string())
}

fn with<R>(f: impl FnOnce(&mut GoogleChat) -> Result<R, String>) -> Result<R, String> {
    let mut guard = lock()?;
    f(guard.as_mut().ok_or("not started")?)
}

fn mode(options: &Value) -> Result<Mode, String> {
    if options["mode"] == "http" {
        return Ok(Mode::Http);
    }
    let field = |key: &str| {
        options[key]
            .as_str()
            .filter(|v| !v.is_empty())
            .ok_or_else(|| {
                format!("options.{key} is required for Pub/Sub (or set mode = \"http\")")
            })
    };
    // `subscription_name` is the key Hermes used.
    let name = field("subscription").or_else(|_| field("subscription_name"))?;
    let project = field("project_id")?;
    let subscription = if name.starts_with("projects/") {
        name.to_owned()
    } else {
        format!("projects/{project}/subscriptions/{name}")
    };
    Ok(Mode::PubSub {
        subscription,
        max: options["max_messages"].as_u64().unwrap_or(1).max(1),
    })
}

fn start(options: &Value) -> Result<GoogleChat, String> {
    let mode = mode(options)?;
    if host::secret(auth::SERVICE_ACCOUNT).is_none() {
        return Err(format!(
            "secret {} (the service-account key) is not set",
            auth::SERVICE_ACCOUNT
        ));
    }
    Ok(GoogleChat {
        auth: auth::Auth::new(options, mode == Mode::Http)?,
        settings: Settings {
            mode,
            typing_indicator: options["typing_indicator"].as_bool().unwrap_or(true),
            oauth_client_id: options["oauth_client_id"].as_str().map(str::to_owned),
            redirect_uri: options["redirect_uri"]
                .as_str()
                .unwrap_or("http://localhost:8085/")
                .to_owned(),
        },
        inbox: Vec::new(),
        writes: Vec::new(),
        senders: HashMap::new(),
        thinking: HashMap::new(),
        cards: Vec::new(),
        polls: 0,
        clicks: 0,
    })
}

impl GoogleChat {
    fn poll(&mut self) -> String {
        self.polls += 1;
        let pulling = matches!(self.settings.mode, Mode::PubSub { .. });
        if pulling
            && self.polls % PULL_POLLS == 1
            && let Err(why) = self.pull()
        {
            host::warn(&format!("Google Chat Pub/Sub pull failed: {why}"));
        }
        let mut items: Vec<Value> = self.inbox.drain(..).map(|m| m.to_value()).collect();
        items.append(&mut self.writes);
        Value::Array(items).to_string()
    }

    fn callback(&mut self, request: &HttpRequest) -> HttpResponse {
        let authorization = adapter_kit::header(request, "authorization").unwrap_or_default();
        if let Err(why) = self.auth.verify_callback(authorization) {
            host::warn(&format!("refused a Google Chat callback: {why}"));
            return respond(401, "text/plain", "unauthorized");
        }
        let Ok(event) = serde_json::from_slice::<Value>(&request.body) else {
            return respond(400, "text/plain", "not an event");
        };
        self.event(&event);
        respond(200, "application/json", "{}")
    }
}

struct Adapter;

impl Guest for Adapter {
    fn descriptor() -> String {
        json!({
            "platform": "google_chat",
            "max_message_length": 4000,
            "length_unit": "chars",
            "splits_long_messages": false,
            "supports_edit": true,
            "supports_delete": true,
            "supports_typing": true,
            "supports_reactions": false,
            "supports_threads": true,
            "interactive": "native",
            "media_in": ["image", "audio", "video", "document"],
            "media_out": ["image", "audio", "video", "document"],
            "delivery_receipts": false,
            "read_receipts": false,
            "streaming_tts": false,
            "markup": "plain",
            "binds_http": true,
            "writable_secrets": [auth::USER_TOKENS],
        })
        .to_string()
    }

    fn start(options: String) -> Result<(), String> {
        let options: Value = serde_json::from_str(&options).map_err(|e| e.to_string())?;
        *lock()? = Some(start(&options)?);
        Ok(())
    }

    fn stop() {
        if let Ok(mut chat) = lock() {
            *chat = None;
        }
    }

    fn poll() -> Result<String, String> {
        clock::tick();
        with(|c| Ok(c.poll()))
    }

    fn send(message: String) -> Result<String, String> {
        let out = Outgoing::parse(&message)?;
        with(|c| match out.op.as_str() {
            "" => c.send(&out),
            "edit" => c.edit(&out).map(|()| String::new()),
            "delete" => c.delete(&out).map(|()| String::new()),
            "typing" => c.typing(&out).map(|()| String::new()),
            _ => Err("unsupported".to_owned()),
        })
    }

    fn chat_info(chat: String) -> Result<String, String> {
        with(|c| c.chat_info(&chat))
    }

    fn handle_http(request: HttpRequest) -> HttpResponse {
        if request.method != "POST" {
            return respond(405, "text/plain", "");
        }
        let Ok(mut guard) = lock() else {
            return respond(503, "text/plain", "busy");
        };
        match guard.as_mut() {
            Some(chat) if chat.settings.mode == Mode::Http => chat.callback(&request),
            Some(_) => respond(404, "text/plain", "this adapter pulls from Pub/Sub"),
            None => respond(503, "text/plain", "not started"),
        }
    }

    fn authorize_sender(_inbound: String) -> String {
        // The gateway's allowlists (emails) decide.
        "unknown".to_owned()
    }
}

adapter_kit::bindings::export_adapter!(Adapter);

/// `space|thread`, the key of a working-state message.
fn thread_key(space: &str, thread: Option<&str>) -> String {
    format!("{space}|{}", thread.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pubsub_needs_a_project_and_subscription() {
        let m = mode(&json!({"project_id": "p", "subscription": "s", "max_messages": 3})).unwrap();
        assert_eq!(
            m,
            Mode::PubSub {
                subscription: "projects/p/subscriptions/s".into(),
                max: 3
            }
        );
        assert!(mode(&json!({"project_id": "p"})).is_err());
        assert!(mode(&json!({"project_id": "p", "subscription_name": "s"})).is_ok());
        assert_eq!(mode(&json!({"mode": "http"})).unwrap(), Mode::Http);
    }
}

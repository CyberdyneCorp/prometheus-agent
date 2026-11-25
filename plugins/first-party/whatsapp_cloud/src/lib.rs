//! The WhatsApp Cloud API adapter (spec: platform-adapters, WhatsApp Cloud
//! API adapter). Meta's webhooks arrive on the shared listener: the
//! subscription handshake echoes `hub.challenge` only for the verify token
//! (the secret `options.verify_token_secret` names, 403 otherwise), and
//! every notification must carry `X-Hub-Signature-256`, the HMAC-SHA256 of
//! the raw body with the app secret (401 on a mismatch, 503 while the app
//! secret is not set). Messages for another phone number id are ignored.
//!
//! Sends go through the Graph API (`options.api_version`, default
//! `v20.0`) for `options.phone_number_id` (15 to 17 digits). Choices are
//! interactive messages ([`outbound`]); inbound messages are marked read
//! with a typing indicator; voice replies go up as OGG/Opus voice notes
//! (the gateway converts speech to Opus first). Outside the 24-hour
//! customer-service window (Graph error 131047) the template
//! `options.reengagement_template` is sent instead and the reply is held
//! until the user answers it; without a template the send fails as
//! "WhatsApp 24-hour window closed".

mod inbound;
mod outbound;

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use adapter_kit::message::{self, Inbound, Outgoing};
use adapter_kit::{Guest, HttpRequest, HttpResponse, clock, host, respond, sign};
use serde_json::Value;

const TOKEN: &str = "WHATSAPP_CLOUD_ACCESS_TOKEN";
const APP_SECRET: &str = "WHATSAPP_CLOUD_APP_SECRET";
const VERIFY_SECRET: &str = "WHATSAPP_CLOUD_VERIFY_TOKEN";

/// What the adapter was started with.
#[derive(Debug, Clone)]
struct Settings {
    /// `https://graph.facebook.com/<version>`.
    graph: String,
    phone_number_id: String,
    verify_secret: String,
    template: Option<String>,
    template_language: String,
}

/// A choice message waiting for its answer: what each number stands for.
#[derive(Debug, Clone)]
struct Choices {
    chat: String,
    data: Vec<String>,
}

#[derive(Default)]
struct State {
    settings: Option<Settings>,
    /// Webhook messages not yet turned into inbound ones (media is
    /// downloaded at poll, not while Meta waits for the webhook answer).
    raw: Vec<(Value, Option<String>)>,
    inbox: Vec<Inbound>,
    /// The last inbound message id of each chat (read marks, typing).
    last: HashMap<String, String>,
    /// Pending choice messages, newest last.
    choices: Vec<Choices>,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

fn lock() -> Result<MutexGuard<'static, Option<State>>, String> {
    STATE.lock().map_err(|e| e.to_string())
}

/// Runs `f` on the started adapter's state.
fn with<R>(f: impl FnOnce(&mut State) -> Result<R, String>) -> Result<R, String> {
    let mut guard = lock()?;
    let state = guard.as_mut().ok_or("not started")?;
    f(state)
}

fn settings() -> Result<Settings, String> {
    with(|s| s.settings.clone().ok_or_else(|| "not started".to_owned()))
}

fn auth() -> Result<String, String> {
    host::secret(TOKEN)
        .map(|t| format!("Bearer {t}"))
        .ok_or_else(|| format!("permanent: secret {TOKEN} is not set"))
}

/// Whether `id` is a phone number id: 15 to 17 digits.
fn valid_phone_number_id(id: &str) -> bool {
    (15..=17).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_digit())
}

fn start_settings(options: &Value) -> Result<Settings, String> {
    let phone_number_id = options["phone_number_id"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| options["phone_number_id"].as_u64().map(|n| n.to_string()))
        .ok_or("options.phone_number_id is required")?;
    if !valid_phone_number_id(&phone_number_id) {
        return Err(format!(
            "options.phone_number_id '{phone_number_id}' is not a phone number id (15 to 17 digits)"
        ));
    }
    if host::secret(TOKEN).is_none() {
        return Err(format!("secret {TOKEN} is not set"));
    }
    let base = options["api_base"]
        .as_str()
        .unwrap_or("https://graph.facebook.com")
        .trim_end_matches('/');
    let version = options["api_version"].as_str().unwrap_or("v20.0");
    Ok(Settings {
        graph: format!("{base}/{version}"),
        phone_number_id,
        verify_secret: options["verify_token_secret"]
            .as_str()
            .unwrap_or(VERIFY_SECRET)
            .to_owned(),
        template: options["reengagement_template"]
            .as_str()
            .filter(|t| !t.is_empty())
            .map(str::to_owned),
        template_language: options["template_language"]
            .as_str()
            .unwrap_or("en_US")
            .to_owned(),
    })
}

/// The subscription handshake: `hub.challenge` for the right token only.
fn handshake(path: &str) -> HttpResponse {
    let query = adapter_kit::query(path);
    let field = |k: &str| {
        query
            .iter()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.as_str())
    };
    let Ok(s) = settings() else {
        return respond(503, "text/plain", "not started");
    };
    let expected = host::secret(&s.verify_secret).unwrap_or_default();
    let given = field("hub.verify_token").unwrap_or_default();
    let matches = !expected.is_empty() && sign::same(expected.as_bytes(), given.as_bytes());
    match (field("hub.mode"), field("hub.challenge")) {
        (Some("subscribe"), Some(challenge)) if matches => respond(200, "text/plain", challenge),
        _ => respond(403, "text/plain", "verify token mismatch"),
    }
}

/// A notification: signed with the app secret.
fn notification(request: &HttpRequest) -> HttpResponse {
    let Some(secret) = host::secret(APP_SECRET) else {
        return respond(503, "text/plain", "WHATSAPP_CLOUD_APP_SECRET is not set");
    };
    let expected = format!(
        "sha256={}",
        sign::hex(&sign::hmac_sha256(secret.as_bytes(), &request.body))
    );
    let given = adapter_kit::header(request, "x-hub-signature-256").unwrap_or_default();
    if !sign::same(expected.as_bytes(), given.to_ascii_lowercase().as_bytes()) {
        return respond(401, "text/plain", "bad signature");
    }
    let body: Value = serde_json::from_slice(&request.body).unwrap_or_default();
    match with(|state| {
        inbound::accept(state, &body);
        Ok(())
    }) {
        Ok(()) => respond(200, "text/plain", "ok"),
        Err(_) => respond(503, "text/plain", "not started"),
    }
}

struct Adapter;

impl Guest for Adapter {
    fn descriptor() -> String {
        serde_json::json!({
            "platform": "whatsapp_cloud",
            "max_message_length": 4096,
            "length_unit": "chars",
            "splits_long_messages": false,
            "supports_edit": false,
            "supports_delete": false,
            "supports_typing": true,
            "supports_reactions": false,
            "supports_threads": false,
            "interactive": "native",
            "media_in": ["image", "audio", "voice", "video", "document"],
            "media_out": ["image", "audio", "voice", "video", "document"],
            "delivery_receipts": false,
            "read_receipts": false,
            "streaming_tts": false,
            "markup": "plain",
            "binds_http": true,
        })
        .to_string()
    }

    fn start(options: String) -> Result<(), String> {
        let options: Value = serde_json::from_str(&options).map_err(|e| e.to_string())?;
        let settings = start_settings(&options)?;
        *lock()? = Some(State {
            settings: Some(settings),
            ..State::default()
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
        let raw = with(|s| Ok(std::mem::take(&mut s.raw)))?;
        let settings = settings()?;
        let mut ready = Vec::new();
        for (message, name) in raw {
            if let Some(inbound) = inbound::normalize(&settings, &message, name) {
                ready.push(inbound);
            }
        }
        let inbox = with(|s| {
            for inbound in &mut ready {
                inbound::answer_choice(s, inbound);
            }
            s.inbox.extend(ready);
            Ok(std::mem::take(&mut s.inbox))
        })?;
        for inbound in &inbox {
            outbound::held_after_reply(&settings, &inbound.chat);
        }
        Ok(message::batch(&inbox))
    }

    fn send(message: String) -> Result<String, String> {
        let out = Outgoing::parse(&message)?;
        let settings = settings()?;
        match out.op.as_str() {
            "" => outbound::send(&settings, &out),
            "typing" => outbound::typing(&settings, &out.chat).map(|()| String::new()),
            _ => Err("unsupported".to_owned()),
        }
    }

    fn chat_info(chat: String) -> Result<String, String> {
        Ok(message::chat_info(&chat, "dm"))
    }

    fn handle_http(request: HttpRequest) -> HttpResponse {
        match request.method.as_str() {
            "GET" => handshake(&request.path),
            "POST" => notification(&request),
            _ => respond(405, "text/plain", ""),
        }
    }

    fn authorize_sender(_inbound: String) -> String {
        // The gateway's allowlists decide.
        "unknown".to_owned()
    }
}

adapter_kit::bindings::export_adapter!(Adapter);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phone_number_ids_have_15_to_17_digits() {
        assert!(valid_phone_number_id("106540352242922"));
        assert!(valid_phone_number_id("10654035224292211"));
        assert!(!valid_phone_number_id("1065403522429"));
        assert!(!valid_phone_number_id("10654035224292a"));
    }
}

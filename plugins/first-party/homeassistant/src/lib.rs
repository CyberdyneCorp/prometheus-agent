//! The Home Assistant adapter (spec: platform-adapters, Home Assistant
//! adapter and toolset): subscribed to `state_changed` over the WebSocket
//! API at `options.url` with the `HASS_TOKEN` secret (a ping every 30 s, a
//! lost connection reopened after 5, 10, 20, 40 then 60 s), forwarding
//! only entities matching `watch_domains`, `watch_entities` or
//! `watch_all` (default off; a WARN when nothing is watched) and not in
//! `ignore_entities`, at most once per entity per `cooldown_secs`
//! (default 30). Events come from sender `homeassistant` in chat
//! `events`; a reply there is a persistent notification, and any other
//! chat is a notify target (`homeassistant:<target>` cron deliveries).

use std::collections::HashMap;
use std::sync::Mutex;

use adapter_kit::bridge::Bridge;
use adapter_kit::message::{self, Inbound, Outgoing};
use adapter_kit::policy::strings;
use adapter_kit::{Guest, HttpRequest, HttpResponse, clock, host, net, respond};
use serde_json::{Value, json};

const TOKEN: &str = "HASS_TOKEN";
/// The chat state changes arrive in (and replies go to as notifications).
const EVENTS: &str = "events";
/// Who state changes come from.
const SENDER: &str = "homeassistant";
const PING_SECS: u64 = 30;
const BACKOFF_SECS: [u64; 5] = [5, 10, 20, 40, 60];

/// Which state changes are forwarded.
#[derive(Debug, Default)]
struct Watch {
    domains: Vec<String>,
    entities: Vec<String>,
    all: bool,
    ignored: Vec<String>,
    cooldown_secs: u64,
    /// `{entity_id}`, `{name}`, `{old}`, `{new}` in the event text.
    template: String,
}

impl Watch {
    fn from(options: &Value) -> Self {
        Self {
            domains: strings(options, "watch_domains"),
            entities: strings(options, "watch_entities"),
            all: options["watch_all"].as_bool().unwrap_or(false),
            ignored: strings(options, "ignore_entities"),
            cooldown_secs: options["cooldown_secs"].as_u64().unwrap_or(30),
            template: options["event_template"]
                .as_str()
                .unwrap_or("{name} ({entity_id}) changed from {old} to {new}")
                .to_owned(),
        }
    }

    fn is_empty(&self) -> bool {
        !self.all && self.domains.is_empty() && self.entities.is_empty()
    }

    fn matches(&self, entity: &str) -> bool {
        if self.ignored.iter().any(|e| e == entity) {
            return false;
        }
        let domain = entity.split('.').next().unwrap_or_default();
        self.all || self.entities.iter().any(|e| e == entity) || self.domains.iter().any(|d| d == domain)
    }
}

struct Session {
    bridge: Bridge,
    url: String,
    connection: Option<String>,
    authenticated: bool,
    next_id: u64,
    last_ping_ms: u64,
    /// When the connection was lost, and how many reopen attempts failed.
    lost_at_ms: u64,
    attempts: usize,
    /// Entity -> when it was last forwarded.
    forwarded: HashMap<String, u64>,
    watch: Watch,
    sent: u64,
}

static SESSION: Mutex<Option<Session>> = Mutex::new(None);

fn websocket_url(url: &str) -> String {
    let base = url.trim_end_matches('/');
    let ws = base
        .strip_prefix("https://")
        .map(|rest| format!("wss://{rest}"))
        .or_else(|| base.strip_prefix("http://").map(|rest| format!("ws://{rest}")))
        .unwrap_or_else(|| base.to_owned());
    format!("{ws}/api/websocket")
}

impl Session {
    fn send_json(&mut self, value: &Value) -> Result<(), String> {
        let id = self.connection.clone().ok_or("not connected")?;
        self.bridge.send(&id, &value.to_string())
    }

    fn request(&mut self, mut value: Value) -> Result<(), String> {
        self.next_id += 1;
        value["id"] = json!(self.next_id);
        self.send_json(&value)
    }

    fn open(&mut self) -> Result<(), String> {
        let id = self.bridge.open(&websocket_url(&self.url), &[])?;
        self.connection = Some(id);
        self.authenticated = false;
        self.next_id = 0;
        Ok(())
    }

    fn lost(&mut self) {
        if let Some(id) = self.connection.take() {
            self.bridge.close(&id);
        }
        self.authenticated = false;
        self.lost_at_ms = clock::now_ms();
    }

    /// Reopens a lost connection once its backoff has passed.
    fn reopen_when_due(&mut self) {
        let wait = BACKOFF_SECS[self.attempts.min(BACKOFF_SECS.len() - 1)];
        if self.connection.is_none() && clock::elapsed(self.lost_at_ms, wait) {
            self.attempts += 1;
            if let Err(why) = self.open() {
                host::warn(&format!("Home Assistant is unreachable: {why}"));
                self.lost_at_ms = clock::now_ms();
            }
        }
    }

    fn state_changed(&mut self, event: &Value) -> Option<Inbound> {
        let data = &event["data"];
        let entity = data["entity_id"].as_str()?;
        if !self.watch.matches(entity) {
            return None;
        }
        let now = clock::now_ms();
        if let Some(last) = self.forwarded.get(entity)
            && now.saturating_sub(*last) < self.watch.cooldown_secs * 1000
        {
            return None;
        }
        self.forwarded.insert(entity.to_owned(), now);
        let state = |which: &str| data[which]["state"].as_str().unwrap_or("unknown").to_owned();
        let name = data["new_state"]["attributes"]["friendly_name"]
            .as_str()
            .unwrap_or(entity);
        let text = self
            .watch
            .template
            .replace("{entity_id}", entity)
            .replace("{name}", name)
            .replace("{old}", &state("old_state"))
            .replace("{new}", &state("new_state"));
        let id = event["context"]["id"]
            .as_str()
            .or_else(|| data["new_state"]["context"]["id"].as_str())
            .map_or_else(|| format!("{entity}@{now}"), str::to_owned);
        Some(Inbound::text(&id, EVENTS, "dm", SENDER, &text))
    }

    fn handle(&mut self, frame: &str, inbound: &mut Vec<Inbound>) -> Result<(), String> {
        let message: Value = serde_json::from_str(frame).unwrap_or(Value::Null);
        match message["type"].as_str() {
            Some("auth_required") => {
                let token = host::secret(TOKEN).ok_or_else(|| format!("secret {TOKEN} is not set"))?;
                self.send_json(&json!({"type": "auth", "access_token": token}))
            }
            Some("auth_ok") => {
                self.authenticated = true;
                self.attempts = 0;
                self.last_ping_ms = clock::now_ms();
                self.request(json!({"type": "subscribe_events", "event_type": "state_changed"}))
            }
            Some("auth_invalid") => Err(format!(
                "Home Assistant refused {TOKEN}: {}",
                message["message"].as_str().unwrap_or("invalid token")
            )),
            Some("event") if message["event"]["event_type"] == "state_changed" => {
                inbound.extend(self.state_changed(&message["event"]));
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn api(&self, path: &str, body: &Value) -> Result<(), String> {
        let token = host::secret(TOKEN).ok_or_else(|| format!("permanent: secret {TOKEN} is not set"))?;
        let auth = format!("Bearer {token}");
        let url = format!("{}{path}", self.url.trim_end_matches('/'));
        net::json("POST", &url, &[("authorization", &auth)], body).map(|_| ())
    }
}

fn valid_target(target: &str) -> bool {
    !target.is_empty() && target.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

struct Adapter;

impl Guest for Adapter {
    fn descriptor() -> String {
        json!({
            "platform": "homeassistant",
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
            "markup": "markdown",
            "binds_http": false,
            "enforces_own_access_policy": "allowlist",
        })
        .to_string()
    }

    fn start(options: String) -> Result<(), String> {
        let options: Value = serde_json::from_str(&options).map_err(|e| e.to_string())?;
        let url = options["url"]
            .as_str()
            .unwrap_or("http://homeassistant.local:8123")
            .to_owned();
        let watch = Watch::from(&options);
        if watch.is_empty() {
            host::warn(
                "homeassistant: no watch_domains, watch_entities or watch_all is set; no state change will be forwarded",
            );
        }
        let mut session = Session {
            bridge: Bridge::from_options(&options)?,
            url,
            connection: None,
            authenticated: false,
            next_id: 0,
            last_ping_ms: 0,
            lost_at_ms: 0,
            attempts: 0,
            forwarded: HashMap::new(),
            watch,
            sent: 0,
        };
        let mut slot = SESSION.lock().map_err(|e| e.to_string())?;
        if let Some(mut old) = slot.take() {
            old.lost();
        }
        session.open()?;
        *slot = Some(session);
        Ok(())
    }

    fn stop() {
        if let Ok(mut slot) = SESSION.lock()
            && let Some(mut session) = slot.take()
        {
            session.lost();
        }
    }

    fn poll() -> Result<String, String> {
        clock::tick();
        let mut slot = SESSION.lock().map_err(|e| e.to_string())?;
        let session = slot.as_mut().ok_or("not started")?;
        session.reopen_when_due();
        let Some(id) = session.connection.clone() else {
            return Ok("[]".to_owned());
        };
        let received = session.bridge.recv(&id)?;
        let mut inbound = Vec::new();
        for frame in &received.frames {
            session.handle(frame, &mut inbound)?;
        }
        if !received.open {
            host::warn(&format!(
                "the Home Assistant connection closed: {}",
                received.error.as_deref().unwrap_or("by the server")
            ));
            session.lost();
        } else if session.authenticated && clock::elapsed(session.last_ping_ms, PING_SECS) {
            session.last_ping_ms = clock::now_ms();
            session.request(json!({"type": "ping"}))?;
        }
        Ok(message::batch(&inbound))
    }

    fn send(message: String) -> Result<String, String> {
        let out = Outgoing::parse(&message)?;
        let mut slot = SESSION.lock().map_err(|e| e.to_string())?;
        let session = slot.as_mut().ok_or("not started")?;
        if out.chat.is_empty() || out.chat == EVENTS {
            session.api(
                "/api/services/persistent_notification/create",
                &json!({"title": "Prometheus", "message": out.text}),
            )?;
        } else if valid_target(&out.chat) {
            session.api(
                &format!("/api/services/notify/{}", out.chat),
                &json!({"message": out.text}),
            )?;
        } else {
            return Err(format!("permanent: '{}' is not a notify target", out.chat));
        }
        session.sent += 1;
        Ok(format!("notification-{}", session.sent))
    }

    fn chat_info(chat: String) -> Result<String, String> {
        let name = if chat == EVENTS { "Home Assistant" } else { chat.as_str() };
        Ok(message::chat_info(name, "dm"))
    }

    fn handle_http(_request: HttpRequest) -> HttpResponse {
        respond(404, "text/plain", "")
    }

    /// State changes are authenticated by the token the adapter connects
    /// with; nobody else writes in its chats.
    fn authorize_sender(inbound: String) -> String {
        let inbound: Value = serde_json::from_str(&inbound).unwrap_or(Value::Null);
        if inbound["sender"] == SENDER { "allow" } else { "deny" }.to_owned()
    }
}

adapter_kit::bindings::export_adapter!(Adapter);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_and_urls() {
        let watch = Watch::from(&json!({"watch_domains": ["light"], "ignore_entities": ["light.hall"]}));
        assert!(watch.matches("light.kitchen"));
        assert!(!watch.matches("light.hall"));
        assert!(!watch.matches("switch.fan"));
        assert!(Watch::from(&json!({})).is_empty());
        assert_eq!(websocket_url("https://ha.example:8123/"), "wss://ha.example:8123/api/websocket");
        assert!(valid_target("mobile_app_phone") && !valid_target("../x"));
    }
}

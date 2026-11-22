//! The Feishu and Lark adapter (spec: platform-adapters, Feishu and Lark
//! adapter; Feishu document comments and meeting invitations). Events
//! arrive over Feishu's long connection (`connection_mode = "websocket"`,
//! the default, [`ws`]) or as signed webhooks on the shared listener
//! (`"webhook"`, [`webhook`]); `domain` picks Feishu (`open.feishu.cn`)
//! or Lark (`open.larksuite.com`). Chats are Feishu chat ids (`oc_..`),
//! or a user's open id (`ou_..`) for a direct chat a meeting invitation
//! opened; document comment threads are `comment:..` chats
//! ([`comments`]).
//!
//! Inbound messages pass the group and direct-chat policies ([`access`])
//! and are batched per sender ([`batch`]); card button presses come back
//! as callbacks, each press token at most once in 15 minutes. Replies go
//! out as text, Markdown posts or cards ([`outbound`]).

use std::sync::Mutex;

use adapter_kit::bridge::Bridge;
use adapter_kit::message::{self, InFile, Inbound, Outgoing};
use adapter_kit::policy::Verdict;
use adapter_kit::{Guest, HttpRequest, HttpResponse, clock, host, respond};
use serde_json::{Value, json};

mod access;
mod api;
mod batch;
mod comments;
mod content;
mod frame;
mod meeting;
mod outbound;
mod rules;
mod webhook;
mod ws;

use access::{Access, Admission};
use api::Api;

const SEEN: usize = 2048;
const CARD_DEDUP_MS: u64 = 15 * 60 * 1000;
const CHATS: usize = 1024;

/// How events arrive.
enum Mode {
    Socket(ws::Socket),
    Webhook,
}

/// The running adapter.
pub(crate) struct Feishu {
    pub(crate) api: Api,
    mode: Mode,
    access: Access,
    batches: batch::Batches,
    keys: webhook::Keys,
    guard: webhook::Guard,
    /// The bot's open id.
    bot: String,
    /// Events the webhook queued for the next poll.
    queue: Vec<Value>,
    /// Message and event ids already handled.
    seen: Vec<String>,
    /// Card press tokens and when they were handled.
    pressed: Vec<(String, u64)>,
    /// Chats seen, with their kind.
    chats: Vec<(String, &'static str)>,
    /// Messages sent as cards (edited as cards).
    pub(crate) cards_sent: Vec<String>,
    /// Reactions added: (message, emoji type, reaction id).
    pub(crate) reactions: Vec<(String, &'static str, String)>,
}

static FS: Mutex<Option<Feishu>> = Mutex::new(None);

fn text(value: &Value) -> &str {
    value.as_str().unwrap_or_default()
}

impl Feishu {
    /// Whether `id` was handled already (remembering it otherwise).
    fn repeated(&mut self, id: &str) -> bool {
        if id.is_empty() {
            return false;
        }
        if self.seen.iter().any(|s| s == id) {
            return true;
        }
        self.seen.push(id.to_owned());
        if self.seen.len() > SEEN {
            self.seen.remove(0);
        }
        false
    }

    fn remember_chat(&mut self, chat: &str, kind: &'static str) {
        if self.chats.iter().any(|(c, _)| c == chat) {
            return;
        }
        self.chats.push((chat.to_owned(), kind));
        if self.chats.len() > CHATS {
            self.chats.remove(0);
        }
    }

    /// The files of a message, downloaded (a note in the text for each
    /// one that could not be).
    fn files(
        &mut self,
        message_id: &str,
        refs: &[content::FileRef],
        text: &mut String,
    ) -> Vec<InFile> {
        let mut files = Vec::new();
        for file in refs {
            let path = format!(
                "/open-apis/im/v1/messages/{message_id}/resources/{}?type={}",
                file.key, file.resource
            );
            match self.api.download(&path) {
                Ok(data) => files.push(InFile::bytes(file.kind, &file.name, data)),
                Err(why) => {
                    host::warn(&format!("Feishu: cannot download {}: {why}", file.name));
                    text.push_str(&format!("\n[attachment {} unavailable]", file.name));
                }
            }
        }
        files
    }

    /// An `im.message.receive_v1` event, when admitted.
    fn message(&mut self, event: &Value) -> Option<Inbound> {
        let (msg, sender) = (&event["message"], &event["sender"]);
        let from = text(&sender["sender_id"]["open_id"]).to_owned();
        let id = text(&msg["message_id"]).to_owned();
        if from.is_empty() || from == self.bot || self.repeated(&id) {
            return None;
        }
        let chat = text(&msg["chat_id"]).to_owned();
        let group = text(&msg["chat_type"]) != "p2p";
        let mentions = msg["mentions"].as_array().cloned().unwrap_or_default();
        let parsed = content::parse(
            text(&msg["message_type"]),
            text(&msg["content"]),
            &mentions,
            &self.bot,
        );
        let admitted = if group {
            self.access.group(&chat, &from, parsed.mentioned) == Admission::Admit
        } else {
            self.access.dm(&from) != Verdict::Deny
        };
        if !admitted {
            return None;
        }
        self.remember_chat(&chat, if group { "group" } else { "dm" });
        let mut body = parsed.text;
        let attachments = self.files(&id, &parsed.files, &mut body);
        if body.trim().is_empty() && attachments.is_empty() {
            return None;
        }
        let mut inbound = Inbound::text(
            &id,
            &chat,
            if group { "group" } else { "dm" },
            &from,
            body.trim(),
        );
        // The adapter applied the chat's mention rule already.
        inbound.mentioned = group || parsed.mentioned;
        inbound.is_bot = text(&sender["sender_type"]) == "app";
        inbound.reply_to = Some(text(&msg["parent_id"]).to_owned()).filter(|p| !p.is_empty());
        inbound.attachments = attachments;
        Some(inbound)
    }

    /// A card button press, once per press token in 15 minutes. Both the
    /// `card.action.trigger` event and the older callback body carry the
    /// operator, the token and the chat.
    fn press(&mut self, event: &Value) -> Option<Inbound> {
        let token = text(&event["token"]).to_owned();
        let now = clock::now_ms();
        self.pressed
            .retain(|(_, at)| now.saturating_sub(*at) < CARD_DEDUP_MS);
        if token.is_empty() || self.pressed.iter().any(|(t, _)| *t == token) {
            return None;
        }
        self.pressed.push((token.clone(), now));
        let value = &event["action"]["value"];
        let data = match &value["prometheus"] {
            Value::String(s) => s.clone(),
            _ => value.to_string(),
        };
        let chat = text(&event["context"]["open_chat_id"])
            .to_owned()
            .or_empty(text(&event["open_chat_id"]));
        let operator = text(&event["operator"]["open_id"])
            .to_owned()
            .or_empty(text(&event["open_id"]));
        if chat.is_empty() || operator.is_empty() {
            return None;
        }
        let kind = self
            .chats
            .iter()
            .find(|(c, _)| *c == chat)
            .map_or("group", |(_, k)| *k);
        let mut inbound = Inbound::text(&format!("card-{token}"), &chat, kind, &operator, "");
        inbound.callback = Some(data);
        Some(inbound)
    }

    /// One event: the messages it admits.
    fn event(&mut self, payload: &Value) -> Vec<Inbound> {
        let header = &payload["header"];
        let event_id = text(&header["event_id"]);
        if !event_id.is_empty() && self.repeated(&format!("event:{event_id}")) {
            return Vec::new();
        }
        let event = &payload["event"];
        match text(&header["event_type"]) {
            "im.message.receive_v1" => self
                .message(event)
                .map(|m| self.batches.push(m))
                .unwrap_or_default(),
            "card.action.trigger" => self.press(event).into_iter().collect(),
            "drive.notice.comment_add_v1" => comments::turn(&mut self.api, event, &self.bot)
                .into_iter()
                .collect(),
            "vc.bot.meeting_invited_v1" => meeting::turn(event, text(&header["event_id"]))
                .filter(|m| self.access.dm(&m.sender) != Verdict::Deny)
                .into_iter()
                .collect(),
            // The older card callback body has no header.
            "" if payload["action"].is_object() => self.press(payload).into_iter().collect(),
            _ => Vec::new(),
        }
    }

    fn poll(&mut self) -> Result<Vec<Inbound>, String> {
        let events = match &mut self.mode {
            Mode::Socket(socket) => socket.poll(&self.api)?,
            Mode::Webhook => std::mem::take(&mut self.queue),
        };
        let mut inbound = Vec::new();
        for payload in &events {
            inbound.extend(self.event(payload));
        }
        inbound.extend(self.batches.due());
        Ok(inbound)
    }

    fn send(&mut self, out: &Outgoing) -> Result<String, String> {
        match out.op.as_str() {
            "" => self.send_message(out),
            "edit" => self.edit(out),
            "delete" => self.delete(out),
            "react" => self.react(out),
            _ => Err("unsupported".to_owned()),
        }
    }

    fn chat_info(&mut self, chat: &str) -> Result<String, String> {
        if let Some(thread) = comments::Thread::of(chat) {
            return Ok(message::chat_info(&thread.file_token, "thread"));
        }
        if chat.starts_with("ou_") || chat.starts_with("on_") {
            return Ok(message::chat_info(chat, "dm"));
        }
        let data = self
            .api
            .call("GET", &format!("/open-apis/im/v1/chats/{chat}"), None)?;
        let kind = if text(&data["chat_mode"]) == "p2p" {
            "dm"
        } else {
            "group"
        };
        self.remember_chat(chat, kind);
        Ok(message::chat_info(text(&data["name"]), kind))
    }

    fn handle_http(&mut self, request: &HttpRequest) -> HttpResponse {
        if !matches!(self.mode, Mode::Webhook) {
            return respond(404, "text/plain", "feishu listens on its long connection");
        }
        match self.guard.check(request, &self.keys) {
            Err(refused) => refused,
            Ok(webhook::Checked::Answer(answer)) => answer,
            Ok(webhook::Checked::Event(payload)) => {
                self.queue.push(payload);
                respond(
                    200,
                    "application/json",
                    json!({"code": 0, "msg": "ok"}).to_string(),
                )
            }
        }
    }

    fn authorize(&self, inbound: &Value) -> Verdict {
        let chat = text(&inbound["chat"]);
        let group = self.chats.iter().any(|(c, k)| c == chat && *k == "group");
        if chat.starts_with("comment:") || group {
            // Comment rules and group policies were applied on arrival.
            return Verdict::Allow;
        }
        self.access.dm(text(&inbound["sender"]))
    }
}

/// `a.or_empty(b)`: `a`, or `b` when `a` is empty.
trait OrEmpty {
    fn or_empty(self, other: &str) -> String;
}

impl OrEmpty for String {
    fn or_empty(self, other: &str) -> String {
        if self.is_empty() {
            other.to_owned()
        } else {
            self
        }
    }
}

fn start(options: &Value) -> Result<Feishu, String> {
    let app_id = text(&options["app_id"]);
    if app_id.is_empty() {
        return Err(
            "options.app_id is required (set by `prometheus gateway setup feishu`)".to_owned(),
        );
    }
    let domain = options["domain"].as_str().unwrap_or("feishu");
    if domain != "feishu" && domain != "lark" {
        return Err(format!("options.domain: '{domain}' is not feishu or lark"));
    }
    let base = options["api_base"].as_str().unwrap_or(api::origin(domain));
    let mut api = Api::new(base, app_id);
    let mode = match options["connection_mode"].as_str().unwrap_or("websocket") {
        "websocket" => Mode::Socket(ws::Socket::new(Bridge::from_options(options)?)),
        "webhook" => Mode::Webhook,
        other => {
            return Err(format!(
                "options.connection_mode: '{other}' is not websocket or webhook"
            ));
        }
    };
    let (bot, _name) = api
        .bot()
        .map_err(|e| e.trim_start_matches("permanent: ").to_owned())?;
    let mut feishu = Feishu {
        api,
        mode,
        access: Access::new(options)?,
        batches: batch::Batches::new(batch::Limits::new(options)),
        keys: webhook::Keys {
            token: host::secret("FEISHU_VERIFICATION_TOKEN"),
            encrypt: host::secret("FEISHU_ENCRYPT_KEY"),
        },
        guard: webhook::Guard::default(),
        bot,
        queue: Vec::new(),
        seen: Vec::new(),
        pressed: Vec::new(),
        chats: Vec::new(),
        cards_sent: Vec::new(),
        reactions: Vec::new(),
    };
    if matches!(feishu.mode, Mode::Webhook) && feishu.keys.token.is_none() {
        // Unverified events could carry any sender's open_id.
        return Err(
            "webhook mode needs the secret FEISHU_VERIFICATION_TOKEN (and FEISHU_ENCRYPT_KEY when the app encrypts events): without it anyone who finds the route could post events as any user"
                .to_owned(),
        );
    }
    if let Mode::Socket(socket) = &mut feishu.mode {
        socket
            .connect(&feishu.api)
            .map_err(|e| e.trim_start_matches("permanent: ").to_owned())?;
    }
    Ok(feishu)
}

fn with<T>(f: impl FnOnce(&mut Feishu) -> Result<T, String>) -> Result<T, String> {
    let mut slot = FS.lock().map_err(|e| e.to_string())?;
    f(slot.as_mut().ok_or("not started")?)
}

struct Adapter;

impl Guest for Adapter {
    fn descriptor() -> String {
        json!({
            "platform": "feishu",
            "max_message_length": 4000,
            "length_unit": "chars",
            "splits_long_messages": false,
            "supports_edit": true,
            "supports_delete": true,
            "supports_typing": false,
            "supports_reactions": true,
            "supports_threads": false,
            "interactive": "native",
            "media_in": ["image", "voice", "audio", "video", "document"],
            "media_out": ["image", "voice", "audio", "video", "document"],
            "delivery_receipts": false,
            "read_receipts": false,
            "streaming_tts": false,
            "markup": "markdown",
            "binds_http": true,
            "max_body_bytes": webhook::MAX_BODY,
            "enforces_own_access_policy": "allowlist",
        })
        .to_string()
    }

    fn start(options: String) -> Result<(), String> {
        let options: Value = serde_json::from_str(&options).map_err(|e| e.to_string())?;
        let feishu = start(&options)?;
        let mut slot = FS.lock().map_err(|e| e.to_string())?;
        if let Some(Feishu {
            mode: Mode::Socket(mut old),
            ..
        }) = slot.take()
        {
            old.close();
        }
        *slot = Some(feishu);
        Ok(())
    }

    fn stop() {
        if let Ok(mut slot) = FS.lock()
            && let Some(Feishu {
                mode: Mode::Socket(mut socket),
                ..
            }) = slot.take()
        {
            socket.close();
        }
    }

    fn poll() -> Result<String, String> {
        clock::tick();
        with(Feishu::poll).map(|inbound| message::batch(&inbound))
    }

    fn send(message: String) -> Result<String, String> {
        let out = Outgoing::parse(&message)?;
        with(|fs| fs.send(&out))
    }

    fn chat_info(chat: String) -> Result<String, String> {
        with(|fs| fs.chat_info(&chat))
    }

    fn handle_http(request: HttpRequest) -> HttpResponse {
        with(|fs| Ok(fs.handle_http(&request)))
            .unwrap_or_else(|_| respond(503, "text/plain", "not started"))
    }

    fn authorize_sender(inbound: String) -> String {
        let inbound: Value = serde_json::from_str(&inbound).unwrap_or_default();
        FS.lock()
            .ok()
            .and_then(|slot| slot.as_ref().map(|fs| fs.authorize(&inbound)))
            .unwrap_or(Verdict::Unknown)
            .as_str()
            .to_owned()
    }
}

adapter_kit::bindings::export_adapter!(Adapter);

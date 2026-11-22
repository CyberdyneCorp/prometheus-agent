//! The Yuanbao adapter (spec: platform-adapters, Yuanbao adapter; Feishu
//! and Yuanbao tools). A token from `sign-token` (HMAC-SHA256 of nonce,
//! timestamp, app key and secret, keyed by the secret) binds the gateway
//! WebSocket (protobuf frames, held by the socket bridge in binary mode):
//! connect within 15 s, ping every 30 s, reconnect after 1 to 60 s.
//!
//! Chats are `direct:<account>` and `group:<group code>`. While a turn
//! runs (the gateway's typing signal) a running-status heartbeat goes out
//! every 2 s, and a finished one with the reply. Forwarded chat-history
//! bundles, nested ones included, reach the agent as their senders'
//! lines; stickers as `[sticker: name]`. Media is fetched only from
//! public HTTPS URLs and uploaded to the platform object store.
//! `dm_policy` and `group_policy` default to `open`.
//!
//! The `yuanbao` toolset's calls reach the running adapter as `op =
//! "tool"` sends: group info and members, a DM to a member found by name,
//! sticker search and sending.

use std::sync::Mutex;

use adapter_kit::bridge::Bridge;
use adapter_kit::message::{self, Inbound, Outgoing};
use adapter_kit::policy::{self, Policy, Verdict};
use adapter_kit::{Guest, HttpRequest, HttpResponse, clock, host, net, respond, sign};
use serde_json::{Value, json};

mod inbound;
mod media;
mod proto;
mod stickers;
mod time;
mod tools;

use proto::{Fields, Frame, Writer, cmd_type};

const SECRET: &str = "YUANBAO_APP_SECRET";
const BIZ: &str = "yuanbao_openclaw_proxy";
const PING_SECS: u64 = 30;
const CONNECT_SECS: u64 = 15;
const TYPING_BEAT_MS: u64 = 2000;
const MAX_BACKOFF_SECS: u64 = 60;
const SEEN: usize = 1000;
const ANSWER_ROUNDS: usize = 4000;

struct Auth {
    token: String,
    bot_id: String,
    source: String,
}

struct Yuanbao {
    api: String,
    ws_url: String,
    cos_base: Option<String>,
    app_key: String,
    dm: Policy,
    allow_from: Vec<String>,
    group: Policy,
    group_allow_from: Vec<String>,
    media_allow: Vec<String>,
    bridge: Bridge,
    connection: Option<String>,
    auth: Option<Auth>,
    bound: bool,
    opened_ms: u64,
    ping_ms: u64,
    backoff_secs: u64,
    retry_at_ms: u64,
    seq: u64,
    /// Chats a turn runs in: (chat, last heartbeat, until).
    typing: Vec<(String, u64, u64)>,
    seen: Vec<String>,
    /// The chat of the last admitted message (where tools act by default).
    seen_chat: Option<String>,
    held: Vec<Frame>,
}

static YB: Mutex<Option<Yuanbao>> = Mutex::new(None);

/// The target of a chat id.
enum Target<'a> {
    Direct(&'a str),
    Group(&'a str),
}

fn target(chat: &str) -> Result<Target<'_>, String> {
    if let Some(account) = chat.strip_prefix("direct:") {
        Ok(Target::Direct(account))
    } else if let Some(code) = chat.strip_prefix("group:") {
        Ok(Target::Group(code))
    } else {
        Err(format!(
            "permanent: '{chat}' is not a Yuanbao chat (direct:<account> or group:<code>)"
        ))
    }
}

/// A `MsgBodyElement` of type `kind` with `content`.
fn element(kind: &str, content: &Writer) -> Writer {
    Writer::default().string(1, kind).message(2, content)
}

fn text_element(text: &str) -> Writer {
    element("TIMTextElem", &Writer::default().string(1, text))
}

impl Yuanbao {
    fn next(&mut self, prefix: &str) -> String {
        self.seq += 1;
        format!("{prefix}_{}_{}", self.seq, clock::now_ms())
    }

    fn sign_token(&mut self) -> Result<Auth, String> {
        let secret =
            host::secret(SECRET).ok_or_else(|| format!("permanent: secret {SECRET} is not set"))?;
        let timestamp = time::beijing_timestamp(&self.api)?;
        self.seq += 1;
        let nonce = sign::hex(
            &sign::hmac_sha256(
                secret.as_bytes(),
                format!("{timestamp}{}", self.seq).as_bytes(),
            )[..16],
        );
        let plain = format!("{nonce}{timestamp}{}{secret}", self.app_key);
        let signature = sign::hex(&sign::hmac_sha256(secret.as_bytes(), plain.as_bytes()));
        let answer = net::json(
            "POST",
            &format!("{}/api/v5/robotLogic/sign-token", self.api),
            &[],
            &json!({"app_key": self.app_key, "nonce": nonce, "signature": signature, "timestamp": timestamp}),
        )?;
        if answer["code"].as_i64() != Some(0) {
            return Err(format!("permanent: Yuanbao refused a token: {answer}"));
        }
        let data = &answer["data"];
        Ok(Auth {
            token: data["token"].as_str().unwrap_or_default().to_owned(),
            bot_id: data["bot_id"].as_str().unwrap_or_default().to_owned(),
            source: data["source"].as_str().unwrap_or("bot").to_owned(),
        })
    }

    fn write(&mut self, frame: &Frame) -> Result<(), String> {
        let id = self.connection.clone().ok_or("server: not connected")?;
        self.seq += 1;
        self.bridge
            .send(&id, &sign::base64(&frame.encode(self.seq)))
    }

    fn connect(&mut self) -> Result<(), String> {
        let auth = self.sign_token()?;
        let id = self.bridge.open_binary(&self.ws_url)?;
        self.connection = Some(id);
        self.opened_ms = clock::now_ms();
        self.bound = false;
        let auth_info = Writer::default()
            .string(1, &auth.bot_id)
            .string(2, &auth.source)
            .string(3, &auth.token);
        let device = Writer::default().string(2, "wasm").required(10, "17");
        let request = Writer::default()
            .required(1, "ybBot")
            .message(2, &auth_info)
            .message(3, &device);
        let msg_id = self.next("auth");
        self.auth = Some(auth);
        self.write(&Frame {
            cmd_type: cmd_type::REQUEST,
            cmd: "auth-bind".into(),
            msg_id,
            module: "conn_access".into(),
            data: request.0,
            ..Frame::default()
        })
    }

    fn lost(&mut self) {
        if let Some(id) = self.connection.take() {
            self.bridge.close(&id);
        }
        self.bound = false;
        self.retry_at_ms = clock::now_ms() + self.backoff_secs * 1000;
        self.backoff_secs = (self.backoff_secs * 2).min(MAX_BACKOFF_SECS);
    }

    fn frames(&mut self) -> Result<(Vec<Frame>, bool), String> {
        let id = self.connection.clone().ok_or("server: not connected")?;
        let received = self.bridge.recv(&id)?;
        let frames = received
            .frames
            .iter()
            .filter_map(|f| sign::base64_decode(f).ok())
            .filter_map(|b| Frame::decode(&b))
            .collect();
        Ok((frames, received.open))
    }

    /// Sends a business request and waits for its response: the body.
    fn request(&mut self, method: &str, prefix: &str, body: &Writer) -> Result<Fields, String> {
        let msg_id = self.next(prefix);
        self.write(&Frame {
            cmd_type: cmd_type::REQUEST,
            cmd: method.into(),
            msg_id: msg_id.clone(),
            module: BIZ.into(),
            data: body.0.clone(),
            ..Frame::default()
        })?;
        for _ in 0..ANSWER_ROUNDS {
            let (frames, open) = self.frames()?;
            let mut answer = None;
            for frame in frames {
                if answer.is_none()
                    && frame.cmd_type == cmd_type::RESPONSE
                    && frame.msg_id == msg_id
                {
                    answer = Some(frame);
                } else {
                    self.held.push(frame);
                }
            }
            if let Some(frame) = answer {
                let body = Fields::parse(&frame.data).unwrap_or_default();
                return refusal(method, frame.status, &body).map_or(Ok(body), Err);
            }
            if !open {
                return Err("server: the Yuanbao connection closed".to_owned());
            }
        }
        Err(format!("server: Yuanbao did not answer {method}"))
    }

    fn bot_id(&self) -> String {
        self.auth
            .as_ref()
            .map(|a| a.bot_id.clone())
            .unwrap_or_default()
    }

    /// Sends `body` elements to `chat`.
    fn send_body(&mut self, chat: &str, body: &[Writer]) -> Result<String, String> {
        let from = self.bot_id();
        let msg_id = self.next("msg");
        let mut request = Writer::default().string(1, &msg_id);
        let (method, prefix, field) = match target(chat)? {
            Target::Direct(account) => {
                request = request.required(2, account).string(3, &from);
                ("send_c2c_message", "c2c", 5)
            }
            Target::Group(code) => {
                request = request.required(2, code).string(3, &from);
                ("send_group_message", "grp", 6)
            }
        };
        for element in body {
            request = request.message(field, element);
        }
        self.request(method, prefix, &request)?;
        Ok(msg_id)
    }

    fn heartbeat(&mut self, chat: &str, running: bool) {
        let from = self.bot_id();
        let state = if running { 1 } else { 2 };
        let (method, prefix, body) = match target(chat) {
            Ok(Target::Direct(account)) => (
                "send_private_heartbeat",
                "hb_priv",
                Writer::default()
                    .string(1, &from)
                    .string(2, account)
                    .int(3, state),
            ),
            Ok(Target::Group(code)) => (
                "send_group_heartbeat",
                "hb_grp",
                Writer::default()
                    .string(1, &from)
                    .string(3, code)
                    .int(5, state),
            ),
            Err(_) => return,
        };
        let msg_id = self.next(prefix);
        let _ = self.write(&Frame {
            cmd_type: cmd_type::REQUEST,
            cmd: method.into(),
            msg_id,
            module: BIZ.into(),
            data: body.0,
            ..Frame::default()
        });
    }

    fn admitted(&self, group: bool, chat: &str, sender: &str) -> bool {
        let verdict = if group {
            self.group.verdict(&self.group_allow_from, chat)
        } else {
            self.dm.verdict(&self.allow_from, sender)
        };
        verdict != Verdict::Deny
    }

    fn push(&mut self, frame: &Frame) -> Option<Inbound> {
        if frame.need_ack {
            let _ = self.write(&Frame {
                cmd_type: cmd_type::PUSH_ACK,
                cmd: frame.cmd.clone(),
                msg_id: frame.msg_id.clone(),
                module: frame.module.clone(),
                ..Frame::default()
            });
        }
        let push = inbound::Push::decode(&frame.data)?;
        if push.from.is_empty() || push.from == self.bot_id() || push.command.contains("Recall") {
            return None;
        }
        let group = !push.group_code.is_empty();
        let chat = if group {
            format!("group:{}", push.group_code)
        } else {
            format!("direct:{}", push.from)
        };
        let id = if push.msg_id.is_empty() {
            frame.msg_id.clone()
        } else {
            push.msg_id.clone()
        };
        let place = if group {
            push.group_code.as_str()
        } else {
            chat.as_str()
        };
        if self.seen.contains(&id) || !self.admitted(group, place, &push.from) {
            return None;
        }
        self.seen.push(id.clone());
        if self.seen.len() > SEEN {
            self.seen.remove(0);
        }
        let mut text = inbound::text_of(&push.body);
        let forwarded = inbound::forwarded(&push.body);
        if !forwarded.is_empty() {
            let note = text.trim().to_owned();
            text = format!("Forwarded chat history:\n{}", forwarded.join("\n"));
            if !note.is_empty() {
                text.push_str(&format!("\n\nThe user added: {note}"));
            }
        }
        let files = inbound::files_of(&push.body, &self.media_allow);
        if text.trim().is_empty() && files.is_empty() {
            return None;
        }
        self.seen_chat = Some(chat.clone());
        let mut message = Inbound::text(
            &id,
            &chat,
            if group { "group" } else { "dm" },
            &push.from,
            text.trim(),
        );
        message.sender_name = Some(push.nickname).filter(|n| !n.is_empty());
        message.mentioned = group;
        message.attachments = files;
        Some(message)
    }

    fn handle(&mut self, frame: &Frame, inbound: &mut Vec<Inbound>) -> Result<(), String> {
        match frame.cmd_type {
            cmd_type::RESPONSE if frame.cmd == "auth-bind" => {
                if frame.status != 0 {
                    return Err(format!(
                        "fatal: yuanbao_auth_failed: auth-bind refused ({})",
                        frame.status
                    ));
                }
                self.bound = true;
                self.backoff_secs = 1;
                self.ping_ms = clock::now_ms();
            }
            cmd_type::PUSH if frame.cmd == "kickout" => self.lost(),
            cmd_type::PUSH => inbound.extend(self.push(frame)),
            _ => {}
        }
        Ok(())
    }

    fn poll(&mut self) -> Result<Vec<Inbound>, String> {
        if self.connection.is_none() {
            if clock::now_ms() >= self.retry_at_ms {
                match self.connect() {
                    Err(why) if why.starts_with("permanent:") => {
                        return Err(format!("fatal: yuanbao_auth_failed: {why}"));
                    }
                    Err(_) => self.lost(),
                    Ok(()) => {}
                }
            }
            return Ok(Vec::new());
        }
        let mut inbound = Vec::new();
        for frame in std::mem::take(&mut self.held) {
            self.handle(&frame, &mut inbound)?;
        }
        let (frames, open) = self.frames()?;
        for frame in &frames {
            self.handle(frame, &mut inbound)?;
        }
        if !open || (!self.bound && clock::elapsed(self.opened_ms, CONNECT_SECS)) {
            self.lost();
            return Ok(inbound);
        }
        if self.bound && clock::elapsed(self.ping_ms, PING_SECS) {
            self.ping_ms = clock::now_ms();
            let msg_id = self.next("ping");
            let _ = self.write(&Frame {
                cmd_type: cmd_type::REQUEST,
                cmd: "ping".into(),
                msg_id,
                module: "conn_access".into(),
                ..Frame::default()
            });
        }
        self.beat();
        Ok(inbound)
    }

    /// The running-status heartbeats of the chats a turn runs in.
    fn beat(&mut self) {
        let now = clock::now_ms();
        self.typing.retain(|(_, _, until)| now < *until);
        let due: Vec<String> = self
            .typing
            .iter()
            .filter(|(_, last, _)| now - last >= TYPING_BEAT_MS)
            .map(|(c, ..)| c.clone())
            .collect();
        for chat in due {
            self.heartbeat(&chat, true);
            if let Some(entry) = self.typing.iter_mut().find(|(c, ..)| *c == chat) {
                entry.1 = now;
            }
        }
    }

    fn send(&mut self, out: &Outgoing) -> Result<String, String> {
        match out.op.as_str() {
            "" => {}
            "typing" => {
                let now = clock::now_ms();
                if !self.typing.iter().any(|(c, ..)| *c == out.chat) {
                    self.heartbeat(&out.chat, true);
                    self.typing.push((out.chat.clone(), now, now));
                }
                // The gateway repeats the signal while the turn runs.
                if let Some(entry) = self.typing.iter_mut().find(|(c, ..)| *c == out.chat) {
                    entry.2 = now + 10_000;
                }
                return Ok(String::new());
            }
            "tool" => return tools::call(self, out),
            _ => return Err("unsupported".to_owned()),
        }
        let mut body: Vec<Writer> = Vec::new();
        for file in &out.attachments {
            body.push(media::upload(self, file)?);
        }
        if !out.text.trim().is_empty() {
            body.push(text_element(&out.text));
        }
        if body.is_empty() {
            return Err("permanent: nothing to send".to_owned());
        }
        let id = self.send_body(&out.chat, &body)?;
        if let Some(at) = self.typing.iter().position(|(c, ..)| *c == out.chat) {
            self.typing.remove(at);
            self.heartbeat(&out.chat, false);
        }
        Ok(id)
    }
}

/// Why an answer refused a request, classified for the gateway's retry:
/// a throttle (code 429), a busy gateway (status 5xx, or the retryable
/// code 10099) or a permanent refusal; `None` when it succeeded.
fn refusal(method: &str, status: u64, body: &Fields) -> Option<String> {
    let code = body.int(1);
    if status == 0 && code == 0 {
        return None;
    }
    let why = format!(
        "Yuanbao refused {method} ({}): {}",
        if status == 0 { code } else { status },
        body.string(2)
    );
    Some(match (status, code) {
        (_, 429) => "rate_limited".to_owned(),
        (500..=599, _) | (_, 10099) => format!("server: {why}"),
        _ => format!("permanent: {why}"),
    })
}

fn with<T>(f: impl FnOnce(&mut Yuanbao) -> Result<T, String>) -> Result<T, String> {
    let mut slot = YB.lock().map_err(|e| e.to_string())?;
    f(slot.as_mut().ok_or("not started")?)
}

struct Adapter;

impl Guest for Adapter {
    fn descriptor() -> String {
        json!({
            "platform": "yuanbao",
            "max_message_length": 4000,
            "length_unit": "chars",
            "splits_long_messages": false,
            "supports_edit": false,
            "supports_delete": false,
            "supports_typing": true,
            "supports_reactions": false,
            "supports_threads": false,
            "interactive": "numbered",
            "media_in": ["image", "voice", "video", "document"],
            "media_out": ["image", "document"],
            "delivery_receipts": false,
            "read_receipts": false,
            "streaming_tts": false,
            "markup": "markdown",
            "binds_http": false,
            "enforces_own_access_policy": "allowlist",
            "tool_ops": true,
        })
        .to_string()
    }

    fn start(options: String) -> Result<(), String> {
        let options: Value = serde_json::from_str(&options).map_err(|e| e.to_string())?;
        let app_key = options["app_id"]
            .as_str()
            .or(options["app_key"].as_str())
            .filter(|a| !a.is_empty())
            .ok_or("options.app_id is required")?
            .to_owned();
        let mut yb = Yuanbao {
            api: options["api_domain"]
                .as_str()
                .unwrap_or("https://bot.yuanbao.tencent.com")
                .trim_end_matches('/')
                .to_owned(),
            ws_url: options["ws_url"]
                .as_str()
                .unwrap_or("wss://bot-wss.yuanbao.tencent.com/wss/connection")
                .to_owned(),
            cos_base: options["cos_base"]
                .as_str()
                .map(|b| b.trim_end_matches('/').to_owned()),
            app_key,
            dm: Policy::option(&options, "dm_policy", Policy::Open)?,
            allow_from: policy::strings(&options, "allow_from"),
            group: Policy::option(&options, "group_policy", Policy::Open)?,
            group_allow_from: policy::strings(&options, "group_allow_from"),
            media_allow: policy::strings(&options, "media_url_prefixes"),
            bridge: Bridge::from_options(&options)?,
            connection: None,
            auth: None,
            bound: false,
            opened_ms: 0,
            ping_ms: 0,
            backoff_secs: 1,
            retry_at_ms: 0,
            seq: 0,
            typing: Vec::new(),
            seen: Vec::new(),
            seen_chat: None,
            held: Vec::new(),
        };
        yb.connect()
            .map_err(|e| e.trim_start_matches("permanent: ").to_owned())?;
        let mut slot = YB.lock().map_err(|e| e.to_string())?;
        if let Some(mut old) = slot.take() {
            old.lost();
        }
        *slot = Some(yb);
        Ok(())
    }

    fn stop() {
        if let Ok(mut slot) = YB.lock()
            && let Some(mut yb) = slot.take()
        {
            yb.lost();
        }
    }

    fn poll() -> Result<String, String> {
        clock::tick();
        with(Yuanbao::poll).map(|inbound| message::batch(&inbound))
    }

    fn send(message: String) -> Result<String, String> {
        let out = Outgoing::parse(&message)?;
        with(|yb| yb.send(&out))
    }

    fn chat_info(chat: String) -> Result<String, String> {
        let kind = if chat.starts_with("group:") {
            "group"
        } else {
            "dm"
        };
        Ok(message::chat_info(&chat, kind))
    }

    fn handle_http(_request: HttpRequest) -> HttpResponse {
        respond(404, "text/plain", "")
    }

    fn authorize_sender(inbound: String) -> String {
        let inbound: Value = serde_json::from_str(&inbound).unwrap_or_default();
        let (chat, sender) = (
            inbound["chat"].as_str().unwrap_or_default(),
            inbound["sender"].as_str().unwrap_or_default(),
        );
        let code = chat.strip_prefix("group:");
        let verdict = YB.lock().ok().and_then(|slot| {
            slot.as_ref().map(|yb| {
                if let Some(code) = code {
                    yb.group.verdict(&yb.group_allow_from, code)
                } else {
                    yb.dm.verdict(&yb.allow_from, sender)
                }
            })
        });
        verdict.unwrap_or(Verdict::Unknown).as_str().to_owned()
    }
}

adapter_kit::bindings::export_adapter!(Adapter);

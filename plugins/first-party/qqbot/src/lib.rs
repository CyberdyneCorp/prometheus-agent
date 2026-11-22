//! The QQ Bot adapter (spec: platform-adapters, QQ Bot adapter): the
//! official API v2. An app access token from `getAppAccessToken` opens
//! the WebSocket gateway (held by the socket bridge): Hello, Identify with
//! the intents for C2C, group @-mentions, guild @-mentions, guild DMs and
//! interactions, heartbeats at the interval Hello names, and Resume after
//! a drop. Replies go over REST, passive (answering the message) while a
//! message id is known.
//!
//! Chats are `c2c:<user openid>`, `group:<group openid>`,
//! `channel:<channel id>` and `dm:<guild id>`. A voice message's text is
//! QQ's own recognition (`asr_refer_text`) when present, else the clip is
//! handed to the gateway, whose speech-to-text transcribes it. With
//! `options.keyboards` approvals carry a message keyboard whose presses
//! come back as interactions (acknowledged at once); otherwise the
//! choices are numbered text and a reply that is just one of the numbers
//! comes back as that choice. Files over 10 MB are uploaded in parts.

use std::sync::Mutex;

use adapter_kit::bridge::Bridge;
use adapter_kit::message::{self, InFile, Inbound, Outgoing};
use adapter_kit::policy::{self, Policy, Verdict};
use adapter_kit::{Guest, HttpRequest, HttpResponse, clock, host, net, respond};
use serde_json::{Value, json};

mod upload;

const SECRET: &str = "QQBOT_CLIENT_SECRET";
const MAX_CHARS: usize = 4000;
const MAX_BACKOFF_SECS: u64 = 60;
const SEEN: usize = 1000;
/// GUILDS | PUBLIC_GUILD_MESSAGES | DIRECT_MESSAGE | C2C_GROUP_AT_MESSAGES | INTERACTION.
const INTENTS: u64 = (1 << 0) | (1 << 30) | (1 << 12) | (1 << 25) | (1 << 26);
const TOKEN_SECS: u64 = 6000;

struct Session {
    id: String,
    seq: Option<u64>,
}

struct QQ {
    api: String,
    token_url: String,
    app_id: String,
    keyboards: bool,
    dm: Policy,
    allow_from: Vec<String>,
    group: Policy,
    group_allow_from: Vec<String>,
    bridge: Bridge,
    connection: Option<String>,
    access: Option<(String, u64)>,
    heartbeat_ms: u64,
    beat_ms: u64,
    session: Option<Session>,
    seq: Option<u64>,
    backoff_secs: u64,
    retry_at_ms: u64,
    /// chat -> the message a passive reply answers, and its reply count.
    last: Vec<(String, String, u64)>,
    seen: Vec<String>,
    /// Choices shown as numbered text: chat -> their data, in order.
    choices: Vec<(String, Vec<String>)>,
    fatal: Option<String>,
}

static QQ_STATE: Mutex<Option<QQ>> = Mutex::new(None);

/// The REST path of a chat's messages (or its `files` upload).
fn path_of(chat: &str, endpoint: &str) -> Result<String, String> {
    let (kind, id) = chat.split_once(':').ok_or_else(|| {
        format!("permanent: '{chat}' is not a QQ chat (c2c:, group:, channel:, dm:)")
    })?;
    Ok(match kind {
        "c2c" => format!("/v2/users/{id}/{endpoint}"),
        "group" => format!("/v2/groups/{id}/{endpoint}"),
        "channel" => format!("/channels/{id}/{endpoint}"),
        "dm" => format!("/dms/{id}/{endpoint}"),
        _ => return Err(format!("permanent: unknown QQ chat kind '{kind}'")),
    })
}

/// A message keyboard: one button per choice, its data the callback.
fn keyboard(buttons: &[adapter_kit::message::Button]) -> Value {
    let rows: Vec<Value> = buttons
        .chunks(3)
        .map(|row| {
            let buttons: Vec<Value> = row
                .iter()
                .enumerate()
                .map(|(i, b)| {
                    json!({
                        "id": format!("{i}-{}", b.data),
                        "render_data": {"label": b.label, "visited_label": b.label, "style": 1},
                        "action": {"type": 1, "permission": {"type": 2}, "data": b.data, "unsupport_tips": b.label},
                    })
                })
                .collect();
            json!({"buttons": buttons})
        })
        .collect();
    json!({"content": {"rows": rows}})
}

fn is_voice(attachment: &Value) -> bool {
    let kind = attachment["content_type"].as_str().unwrap_or_default();
    let name = attachment["filename"]
        .as_str()
        .unwrap_or_default()
        .to_lowercase();
    kind == "voice"
        || kind.starts_with("audio/")
        || name.ends_with(".silk")
        || name.ends_with(".amr")
}

impl QQ {
    fn token(&mut self) -> Result<String, String> {
        if let Some((token, at)) = &self.access
            && !clock::elapsed(*at, TOKEN_SECS)
        {
            return Ok(token.clone());
        }
        let secret =
            host::secret(SECRET).ok_or_else(|| format!("permanent: secret {SECRET} is not set"))?;
        let answer = net::json(
            "POST",
            &self.token_url,
            &[],
            &json!({"appId": self.app_id, "clientSecret": secret}),
        )?;
        let token = answer["access_token"]
            .as_str()
            .ok_or_else(|| format!("permanent: QQ refused an access token: {answer}"))?
            .to_owned();
        self.access = Some((token.clone(), clock::now_ms()));
        Ok(token)
    }

    fn rest(&mut self, method: &str, path: &str, body: &Value) -> Result<Value, String> {
        let auth = format!("QQBot {}", self.token()?);
        let url = format!("{}{path}", self.api);
        net::json(
            method,
            &url,
            &[("authorization", &auth), ("x-union-appid", &self.app_id)],
            body,
        )
    }

    fn connect(&mut self) -> Result<(), String> {
        let auth = format!("QQBot {}", self.token()?);
        let gateway = net::get(
            &format!("{}/gateway", self.api),
            &[("authorization", &auth)],
        )?;
        let url = gateway["url"]
            .as_str()
            .ok_or("server: QQ answered no gateway url")?;
        self.connection = Some(self.bridge.open(url, &[])?);
        self.heartbeat_ms = 0;
        Ok(())
    }

    fn lost(&mut self) {
        if let Some(id) = self.connection.take() {
            self.bridge.close(&id);
        }
        self.retry_at_ms = clock::now_ms() + self.backoff_secs * 1000;
        self.backoff_secs = (self.backoff_secs * 2).min(MAX_BACKOFF_SECS);
    }

    fn send_frame(&self, frame: &Value) -> Result<(), String> {
        let id = self.connection.clone().ok_or("server: not connected")?;
        self.bridge.send(&id, &frame.to_string())
    }

    fn hello(&mut self, data: &Value) -> Result<(), String> {
        self.heartbeat_ms = data["heartbeat_interval"].as_u64().unwrap_or(41_250);
        self.beat_ms = clock::now_ms();
        let token = format!("QQBot {}", self.token()?);
        let frame = match &self.session {
            Some(session) => {
                json!({"op": 6, "d": {"token": token, "session_id": session.id, "seq": session.seq}})
            }
            None => json!({"op": 2, "d": {"token": token, "intents": INTENTS, "shard": [0, 1]}}),
        };
        self.send_frame(&frame)
    }

    fn admitted(&self, group: bool, chat: &str, sender: &str) -> bool {
        let verdict = if group {
            self.group.verdict(&self.group_allow_from, chat)
        } else {
            self.dm.verdict(&self.allow_from, sender)
        };
        verdict != Verdict::Deny
    }

    /// The text and files of a message: a voice clip with QQ's own
    /// recognition becomes that text, without the clip.
    fn content(data: &Value) -> (String, Vec<InFile>) {
        let mut text = data["content"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .to_owned();
        let mut files = Vec::new();
        for attachment in data["attachments"].as_array().into_iter().flatten() {
            let transcript = attachment["asr_refer_text"]
                .as_str()
                .unwrap_or_default()
                .trim();
            if is_voice(attachment) && !transcript.is_empty() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(transcript);
                continue;
            }
            let Some(url) = attachment["voice_wav_url"]
                .as_str()
                .or(attachment["url"].as_str())
            else {
                continue;
            };
            let url = if url.starts_with("//") {
                format!("https:{url}")
            } else {
                url.to_owned()
            };
            let kind = if is_voice(attachment) {
                "voice"
            } else {
                match attachment["content_type"].as_str().unwrap_or_default() {
                    k if k.starts_with("image") => "image",
                    k if k.starts_with("video") => "video",
                    _ => "document",
                }
            };
            let Ok(response) = net::send("GET", &url, &[], None) else {
                continue;
            };
            if net::classify(&response).is_ok() {
                let name = attachment["filename"].as_str().unwrap_or("file");
                files.push(InFile::bytes(kind, name, response.body));
            }
        }
        (text, files)
    }

    fn message(&mut self, kind: &str, data: &Value) -> Option<Inbound> {
        let author = &data["author"];
        let (chat, sender, chat_type) = match kind {
            "C2C_MESSAGE_CREATE" => {
                let user = author["user_openid"].as_str()?;
                (format!("c2c:{user}"), user, "dm")
            }
            "GROUP_AT_MESSAGE_CREATE" => (
                format!("group:{}", data["group_openid"].as_str()?),
                author["member_openid"].as_str()?,
                "group",
            ),
            "AT_MESSAGE_CREATE" => (
                format!("channel:{}", data["channel_id"].as_str()?),
                author["id"].as_str()?,
                "group",
            ),
            "DIRECT_MESSAGE_CREATE" => (
                format!("dm:{}", data["guild_id"].as_str()?),
                author["id"].as_str()?,
                "dm",
            ),
            _ => return None,
        };
        let id = data["id"].as_str()?.to_owned();
        if self.seen.contains(&id) || !self.admitted(chat_type == "group", &chat, sender) {
            return None;
        }
        self.seen.push(id.clone());
        if self.seen.len() > SEEN {
            self.seen.remove(0);
        }
        let (mut text, files) = Self::content(data);
        if chat_type == "group" && text.starts_with("<@") {
            text = text
                .split_once('>')
                .map_or("", |(_, rest)| rest)
                .trim()
                .to_owned();
        }
        if text.is_empty() && files.is_empty() {
            return None;
        }
        self.last.retain(|(c, ..)| *c != chat);
        self.last.push((chat.clone(), id.clone(), 0));
        let mut inbound = Inbound::text(&id, &chat, chat_type, sender, &text);
        inbound.mentioned = chat_type == "group";
        inbound.attachments = files;
        inbound.callback = self.choice(&chat, &text);
        Some(inbound)
    }

    /// The choice a numbered reply picks, taking the chat's choices.
    fn choice(&mut self, chat: &str, text: &str) -> Option<String> {
        let n: usize = text.trim().parse().ok()?;
        let at = self.choices.iter().position(|(c, _)| c == chat)?;
        let data = self.choices[at].1.get(n.checked_sub(1)?)?.clone();
        self.choices.remove(at);
        Some(data)
    }

    /// A pressed keyboard button: acknowledged, and its data as a callback.
    fn interaction(&mut self, data: &Value) -> Option<Inbound> {
        let id = data["id"].as_str()?.to_owned();
        let _ = self.rest("PUT", &format!("/interactions/{id}"), &json!({"code": 0}));
        let button = data["data"]["resolved"]["button_data"].as_str()?;
        let (chat, sender, chat_type) =
            match (data["group_openid"].as_str(), data["user_openid"].as_str()) {
                (Some(group), _) => (
                    format!("group:{group}"),
                    data["group_member_openid"].as_str()?,
                    "group",
                ),
                (None, Some(user)) => (format!("c2c:{user}"), user, "dm"),
                _ => return None,
            };
        let mut inbound = Inbound::text(&id, &chat, chat_type, sender, "");
        inbound.callback = Some(button.to_owned());
        Some(inbound)
    }

    fn frame(&mut self, frame: &str, inbound: &mut Vec<Inbound>) -> Result<(), String> {
        let Ok(event) = serde_json::from_str::<Value>(frame) else {
            return Ok(());
        };
        if let Some(seq) = event["s"].as_u64() {
            self.seq = Some(seq);
            if let Some(session) = &mut self.session {
                session.seq = Some(seq);
            }
        }
        match event["op"].as_u64() {
            Some(10) => self.hello(&event["d"])?,
            // Reconnect, or an invalid session (start a new one).
            Some(7) => self.lost(),
            Some(9) => {
                self.session = None;
                self.lost();
            }
            Some(0) => {
                let kind = event["t"].as_str().unwrap_or_default();
                match kind {
                    "READY" => {
                        self.backoff_secs = 2;
                        self.session = event["d"]["session_id"].as_str().map(|id| Session {
                            id: id.to_owned(),
                            seq: self.seq,
                        });
                    }
                    "RESUMED" => self.backoff_secs = 2,
                    "INTERACTION_CREATE" => inbound.extend(self.interaction(&event["d"])),
                    _ => inbound.extend(self.message(kind, &event["d"])),
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// The fields that make a send a passive reply: the message it
    /// answers and a sequence distinct per reply to it.
    fn passive(&mut self, chat: &str, body: &mut Value) {
        if let Some(entry) = self.last.iter_mut().find(|(c, ..)| c == chat) {
            entry.2 += 1;
            body["msg_id"] = json!(entry.1);
            body["msg_seq"] = json!(entry.2);
        } else {
            body["msg_seq"] = json!(clock::now_ms() % 1_000_000_000 + 1);
        }
    }

    fn send(&mut self, out: &Outgoing) -> Result<String, String> {
        if !out.op.is_empty() {
            return Err("unsupported".to_owned());
        }
        let mut last = None;
        for file in &out.attachments {
            let info = upload::upload(self, &out.chat, file)?;
            let mut body = json!({"msg_type": 7, "media": {"file_info": info}});
            self.passive(&out.chat, &mut body);
            let answer = self.rest("POST", &path_of(&out.chat, "messages")?, &body)?;
            last = answer["id"].as_str().map(str::to_owned);
        }
        if out.text.trim().is_empty() {
            return last.ok_or_else(|| "permanent: nothing to send".to_owned());
        }
        let mut body = json!({"content": out.text, "msg_type": 0});
        if self.keyboards && !out.buttons.is_empty() {
            body = json!({"msg_type": 2, "markdown": {"content": out.text}, "keyboard": keyboard(&out.buttons)});
        } else if !out.buttons.is_empty() {
            let mut text = out.text.clone();
            for (i, button) in out.buttons.iter().enumerate() {
                text.push_str(&format!("\n{}. {}", i + 1, button.label));
            }
            text.push_str("\nReply with a number.");
            body["content"] = json!(text);
            self.choices.retain(|(c, _)| *c != out.chat);
            self.choices.push((
                out.chat.clone(),
                out.buttons.iter().map(|b| b.data.clone()).collect(),
            ));
        }
        self.passive(&out.chat, &mut body);
        let answer = self.rest("POST", &path_of(&out.chat, "messages")?, &body)?;
        Ok(answer["id"].as_str().unwrap_or("sent").to_owned())
    }
}

fn poll_gateway(qq: &mut QQ) -> Result<Vec<Inbound>, String> {
    if let Some(why) = qq.fatal.clone() {
        return Err(why);
    }
    let Some(id) = qq.connection.clone() else {
        if clock::now_ms() >= qq.retry_at_ms {
            match qq.connect() {
                Err(why) if why.starts_with("permanent:") => {
                    qq.fatal = Some(format!("fatal: qqbot_unauthorized: {why}"));
                }
                Err(_) => qq.lost(),
                Ok(()) => {}
            }
        }
        return Ok(Vec::new());
    };
    let received = qq.bridge.recv(&id)?;
    let mut inbound = Vec::new();
    for frame in &received.frames {
        qq.frame(frame, &mut inbound)?;
    }
    if !received.open {
        qq.lost();
    } else if qq.heartbeat_ms > 0 && clock::now_ms() - qq.beat_ms >= qq.heartbeat_ms {
        qq.beat_ms = clock::now_ms();
        let _ = qq.send_frame(&json!({"op": 1, "d": qq.seq}));
    }
    Ok(inbound)
}

fn with<T>(f: impl FnOnce(&mut QQ) -> Result<T, String>) -> Result<T, String> {
    let mut slot = QQ_STATE.lock().map_err(|e| e.to_string())?;
    f(slot.as_mut().ok_or("not started")?)
}

struct Adapter;

impl Guest for Adapter {
    fn descriptor() -> String {
        // Buttons reach the plugin either way: without keyboards it
        // numbers them itself and maps a numbered reply back.
        json!({
            "platform": "qqbot",
            "max_message_length": MAX_CHARS,
            "length_unit": "chars",
            "splits_long_messages": false,
            "supports_edit": false,
            "supports_delete": false,
            "supports_typing": false,
            "supports_reactions": false,
            "supports_threads": false,
            "interactive": "native",
            "media_in": ["image", "voice", "video", "document"],
            "media_out": ["image", "voice", "video", "document"],
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
        let app_id = options["app_id"]
            .as_str()
            .map(str::to_owned)
            .or_else(|| options["app_id"].as_u64().map(|n| n.to_string()))
            .filter(|a| !a.is_empty())
            .ok_or("options.app_id is required")?;
        let mut qq = QQ {
            api: options["api_base"]
                .as_str()
                .unwrap_or("https://api.sgroup.qq.com")
                .trim_end_matches('/')
                .to_owned(),
            token_url: options["token_url"]
                .as_str()
                .unwrap_or("https://bots.qq.com/app/getAppAccessToken")
                .to_owned(),
            app_id,
            keyboards: options["keyboards"].as_bool().unwrap_or(false),
            dm: Policy::option(&options, "dm_policy", Policy::Open)?,
            allow_from: policy::strings(&options, "allow_from"),
            group: Policy::option(&options, "group_policy", Policy::Open)?,
            group_allow_from: policy::strings(&options, "group_allow_from"),
            bridge: Bridge::from_options(&options)?,
            connection: None,
            access: None,
            heartbeat_ms: 0,
            beat_ms: 0,
            session: None,
            seq: None,
            backoff_secs: 2,
            retry_at_ms: 0,
            last: Vec::new(),
            seen: Vec::new(),
            choices: Vec::new(),
            fatal: None,
        };
        qq.connect()
            .map_err(|e| e.trim_start_matches("permanent: ").to_owned())?;
        let mut slot = QQ_STATE.lock().map_err(|e| e.to_string())?;
        if let Some(mut old) = slot.take() {
            old.lost();
        }
        *slot = Some(qq);
        Ok(())
    }

    fn stop() {
        if let Ok(mut slot) = QQ_STATE.lock()
            && let Some(mut qq) = slot.take()
        {
            qq.lost();
        }
    }

    fn poll() -> Result<String, String> {
        clock::tick();
        with(poll_gateway).map(|inbound| message::batch(&inbound))
    }

    fn send(message: String) -> Result<String, String> {
        let out = Outgoing::parse(&message)?;
        with(|qq| qq.send(&out))
    }

    fn chat_info(chat: String) -> Result<String, String> {
        let kind = if chat.starts_with("group:") || chat.starts_with("channel:") {
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
        let group = inbound["chat_type"] == "group";
        let verdict = QQ_STATE.lock().ok().and_then(|slot| {
            slot.as_ref().map(|qq| {
                if group {
                    qq.group.verdict(&qq.group_allow_from, chat)
                } else {
                    qq.dm.verdict(&qq.allow_from, sender)
                }
            })
        });
        verdict.unwrap_or(Verdict::Unknown).as_str().to_owned()
    }
}

adapter_kit::bindings::export_adapter!(Adapter);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chats_map_to_their_rest_paths() {
        assert_eq!(
            path_of("c2c:U1", "messages").unwrap(),
            "/v2/users/U1/messages"
        );
        assert_eq!(path_of("group:G1", "files").unwrap(), "/v2/groups/G1/files");
        assert_eq!(
            path_of("channel:9", "messages").unwrap(),
            "/channels/9/messages"
        );
        assert!(path_of("U1", "messages").is_err());
    }

    #[test]
    fn a_platform_transcript_replaces_the_voice_clip() {
        let (text, files) = QQ::content(&json!({
            "content": "",
            "attachments": [{"content_type": "voice", "url": "https://x/v.silk", "asr_refer_text": "turn on the lights"}],
        }));
        assert_eq!(text, "turn on the lights");
        assert!(files.is_empty());
    }
}

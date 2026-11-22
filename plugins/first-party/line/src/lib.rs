//! The LINE adapter (spec: platform-adapters, LINE adapter): Messaging API
//! webhooks on the shared listener verified by `X-Line-Signature`
//! (HMAC-SHA256 of the body with the channel secret, base64; 401 on a
//! mismatch), replies with the event's free single-use reply token while
//! it is fresh (50 s) and through the Push API afterwards.
//!
//! A turn slower than `options.slow_response_secs` (default 45, 0 off)
//! spends the reply token on a "Still thinking" button; the answer is then
//! held until the user presses "Get answer", whose postback brings a fresh
//! reply token that delivers it (no Push charge). LINE takes no binary
//! uploads: sent files are served from `options.public_url` (HTTPS) under
//! `/media/<token>/<name>`, links that live as long as the adapter holds
//! them. The token is an HMAC keyed with the channel secret over the start
//! epoch (counted in the plugin's state, so a restart never repeats one),
//! the file's sequence number, name and content: nobody without the
//! secret can guess a link (the sandbox has no random source). Text goes out in bubbles of 4500 characters, five per API call.
//! `allowed_users`, `allowed_groups` and `allowed_rooms` take `U`, `C` and
//! `R` ids.

use std::sync::Mutex;

use adapter_kit::message::{self, InFile, Inbound, Outgoing};
use adapter_kit::{Guest, HttpRequest, HttpResponse, clock, host, net, policy, respond, sign};
use serde_json::{Value, json};

const SECRET: &str = "LINE_CHANNEL_SECRET";
const TOKEN: &str = "LINE_CHANNEL_ACCESS_TOKEN";
const BUBBLE_CHARS: usize = 4500;
const BUBBLES_PER_CALL: usize = 5;
const REPLY_TOKEN_SECS: u64 = 50;
const GET_ANSWER: &str = "prometheus:get_answer";
/// Sent files kept for LINE to fetch.
const SERVED_FILES: usize = 32;

/// A chat's reply token and what its turn has shown so far.
struct Pending {
    chat: String,
    token: Option<String>,
    received_ms: u64,
    /// The "Still thinking" button was shown: answers wait for the press.
    waiting: bool,
    /// Answers held for the press.
    held: Vec<Value>,
}

struct Settings {
    api: String,
    data_api: String,
    public_url: String,
    slow_secs: u64,
    users: Vec<String>,
    groups: Vec<String>,
    rooms: Vec<String>,
}

#[derive(Default)]
struct Line {
    settings: Option<Settings>,
    pending: Vec<Pending>,
    inbox: Vec<Inbound>,
    /// Served files: (token, name, mime, bytes).
    served: Vec<(String, String, String, Vec<u8>)>,
    served_seq: u64,
    /// This start's number (see the module comment).
    epoch: u64,
}

static LINE: Mutex<Line> = Mutex::new(Line {
    settings: None,
    pending: Vec::new(),
    inbox: Vec::new(),
    served: Vec::new(),
    served_seq: 0,
    epoch: 0,
});

/// The plugin state key counting starts.
const EPOCH_KEY: &str = "media_epoch";

/// The next start's number, recorded so the following start differs.
fn next_epoch() -> u64 {
    let epoch = host::load(EPOCH_KEY)
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
        .wrapping_add(1);
    host::store(EPOCH_KEY, &json!(epoch));
    epoch
}

/// The unguessable link token of a served file.
fn media_token(key: &str, epoch: u64, seq: u64, file: &message::OutFile) -> String {
    let content = sign::hex(&sign::sha256(&file.data));
    let mac = sign::hmac_sha256(
        key.as_bytes(),
        format!("{epoch}:{seq}:{}:{content}", file.name).as_bytes(),
    );
    sign::hex(&mac[..16])
}

fn auth() -> Result<String, String> {
    host::secret(TOKEN)
        .map(|t| format!("Bearer {t}"))
        .ok_or_else(|| format!("permanent: secret {TOKEN} is not set"))
}

/// The ids of `key` that do not start with `prefix`.
fn misprefixed(options: &Value, key: &str, prefix: char) -> Result<Vec<String>, String> {
    let ids = policy::strings(options, key);
    match ids.iter().find(|id| !id.starts_with(prefix)) {
        Some(bad) => Err(format!(
            "options.{key}: '{bad}' is not a LINE id starting with {prefix}"
        )),
        None => Ok(ids),
    }
}

/// `text` in bubbles of at most 4500 characters.
fn bubbles(text: &str) -> Vec<Value> {
    adapter_kit::text::split_chars(text, BUBBLE_CHARS)
        .into_iter()
        .map(|t| json!({"type": "text", "text": t}))
        .collect()
}

fn still_thinking() -> Value {
    json!({
        "type": "template",
        "altText": "Still thinking… tap Get answer when ready.",
        "template": {
            "type": "buttons",
            "text": "Still thinking…",
            "actions": [{"type": "postback", "label": "Get answer", "data": GET_ANSWER}],
        },
    })
}

impl Line {
    fn settings(&self) -> Result<&Settings, String> {
        self.settings
            .as_ref()
            .ok_or_else(|| "not started".to_owned())
    }

    fn pending(&mut self, chat: &str) -> Option<&mut Pending> {
        self.pending.iter_mut().find(|p| p.chat == chat)
    }

    /// A fresh reply token of `chat`, taken (they are single-use).
    fn take_token(&mut self, chat: &str) -> Option<String> {
        let pending = self.pending(chat)?;
        if clock::elapsed(pending.received_ms, REPLY_TOKEN_SECS) {
            pending.token = None;
        }
        pending.token.take()
    }

    fn serve(&mut self, file: &message::OutFile) -> Result<String, String> {
        let base = self.settings()?.public_url.clone();
        let key = host::secret(SECRET).ok_or_else(|| format!("secret {SECRET} is not set"))?;
        self.served_seq += 1;
        let token = media_token(&key, self.epoch, self.served_seq, file);
        let name = adapter_kit::text::percent_encode(&file.name);
        self.served.push((
            token.clone(),
            file.name.clone(),
            file.mime.clone(),
            file.data.clone(),
        ));
        if self.served.len() > SERVED_FILES {
            self.served.remove(0);
        }
        Ok(format!("{base}/media/{token}/{name}"))
    }

    fn file_message(&mut self, file: &message::OutFile) -> Result<Value, String> {
        let url = self.serve(file)?;
        Ok(match file.kind.as_str() {
            "image" if !file.as_document => {
                json!({"type": "image", "originalContentUrl": url, "previewImageUrl": url})
            }
            "video" => json!({"type": "video", "originalContentUrl": url, "previewImageUrl": url}),
            "audio" | "voice" => {
                json!({"type": "audio", "originalContentUrl": url, "duration": 60000})
            }
            // Bots cannot send files: a link to it.
            _ => json!({"type": "text", "text": format!("{} {url}", file.name)}),
        })
    }

    /// Shows "Still thinking" in every chat whose turn passed the
    /// threshold while its reply token is still fresh.
    fn slow_turns(&mut self) -> Result<(), String> {
        let slow = self.settings()?.slow_secs;
        if slow == 0 {
            return Ok(());
        }
        let due: Vec<(String, String)> = self
            .pending
            .iter_mut()
            .filter(|p| !p.waiting && clock::elapsed(p.received_ms, slow))
            .filter(|p| !clock::elapsed(p.received_ms, REPLY_TOKEN_SECS))
            .filter_map(|p| {
                let token = p.token.take()?;
                p.waiting = true;
                Some((p.chat.clone(), token))
            })
            .collect();
        for (chat, token) in due {
            if let Err(why) = self.call(
                "reply",
                json!({"replyToken": token, "messages": [still_thinking()]}),
            ) {
                host::warn(&format!(
                    "LINE did not take the Still thinking button for {chat}: {why}"
                ));
            }
        }
        Ok(())
    }

    fn call(&self, kind: &str, body: Value) -> Result<Value, String> {
        let url = format!("{}/v2/bot/message/{kind}", self.settings()?.api);
        net::json("POST", &url, &[("authorization", &auth()?)], &body)
    }

    /// Delivers `messages` to `chat`: the reply token first, Push for the
    /// rest and once it is spent or stale.
    fn deliver(&mut self, chat: &str, messages: Vec<Value>) -> Result<String, String> {
        let mut groups = messages.chunks(BUBBLES_PER_CALL).map(<[Value]>::to_vec);
        let first = groups.next().unwrap_or_default();
        let replied = match self.take_token(chat) {
            Some(token) => self
                .call("reply", json!({"replyToken": token, "messages": first}))
                .map(|_| true)
                .or_else(|why| {
                    if why.contains("Invalid reply token") {
                        Ok(false)
                    } else {
                        Err(why)
                    }
                })?,
            None => false,
        };
        if !replied {
            self.call("push", json!({"to": chat, "messages": first}))?;
        }
        for group in groups {
            self.call("push", json!({"to": chat, "messages": group}))?;
        }
        self.served_seq += 1;
        Ok(format!("{chat}:{}", self.served_seq))
    }

    fn postback(&mut self, chat: &str, token: &str, data: &str, event: &Value) {
        if data != GET_ANSWER {
            let sender = event["source"]["userId"].as_str().unwrap_or(chat);
            let mut inbound = Inbound::text(
                event["webhookEventId"].as_str().unwrap_or(token),
                chat,
                "dm",
                sender,
                "",
            );
            inbound.callback = Some(data.to_owned());
            self.inbox.push(inbound);
            return;
        }
        let Some(pending) = self.pending(chat) else {
            return;
        };
        let held = std::mem::take(&mut pending.held);
        if held.is_empty() {
            // Not done yet: the fresh token answers when it is.
            pending.token = Some(token.to_owned());
            pending.received_ms = clock::now_ms();
            return;
        }
        self.pending.retain(|p| p.chat != chat);
        self.pending.push(Pending {
            chat: chat.to_owned(),
            token: Some(token.to_owned()),
            received_ms: clock::now_ms(),
            waiting: false,
            held: Vec::new(),
        });
        if let Err(why) = self.deliver(chat, held) {
            host::warn(&format!("LINE did not deliver a held answer: {why}"));
        }
    }

    fn event(&mut self, event: &Value) {
        let source = &event["source"];
        let (chat, kind) = match source["type"].as_str() {
            Some("group") => (source["groupId"].as_str(), "group"),
            Some("room") => (source["roomId"].as_str(), "group"),
            _ => (source["userId"].as_str(), "dm"),
        };
        let Some(chat) = chat.map(str::to_owned) else {
            return;
        };
        let token = event["replyToken"].as_str().unwrap_or_default().to_owned();
        if event["type"] == "postback" {
            self.postback(
                &chat,
                &token,
                event["postback"]["data"].as_str().unwrap_or_default(),
                event,
            );
            return;
        }
        if event["type"] != "message" {
            return;
        }
        let message = &event["message"];
        let sender = source["userId"].as_str().unwrap_or(&chat).to_owned();
        let id = message["id"].as_str().unwrap_or_default().to_owned();
        let text = message["text"].as_str().unwrap_or_default();
        let mut inbound = Inbound::text(&id, &chat, kind, &sender, text);
        if let Some(file) = self.content(message) {
            inbound.attachments.push(file);
        }
        // A group message mentioning the bot is marked so by LINE.
        inbound.mentioned = message["mention"]["mentionees"]
            .as_array()
            .is_some_and(|m| m.iter().any(|x| x["isSelf"] == true));
        self.pending.retain(|p| p.chat != chat);
        self.pending.push(Pending {
            chat,
            token: Some(token).filter(|t| !t.is_empty()),
            received_ms: clock::now_ms(),
            waiting: false,
            held: Vec::new(),
        });
        self.inbox.push(inbound);
    }

    /// An image, video, audio or file message's content.
    fn content(&self, message: &Value) -> Option<InFile> {
        let kind = match message["type"].as_str()? {
            "image" => "image",
            "video" => "video",
            "audio" => "audio",
            "file" => "document",
            _ => return None,
        };
        let id = message["id"].as_str()?;
        let name = message["fileName"].as_str().map_or_else(
            || {
                format!(
                    "{id}.{}",
                    match kind {
                        "image" => "jpg",
                        "video" => "mp4",
                        "audio" => "m4a",
                        _ => "bin",
                    }
                )
            },
            str::to_owned,
        );
        let url = format!(
            "{}/v2/bot/message/{id}/content",
            self.settings().ok()?.data_api
        );
        let auth = auth().ok()?;
        match net::send("GET", &url, &[("authorization", &auth)], None) {
            Ok(response) if response.status < 300 => {
                Some(InFile::bytes(kind, &name, response.body))
            }
            _ => {
                host::warn(&format!("could not download LINE content {id}"));
                None
            }
        }
    }
}

fn lock() -> Result<std::sync::MutexGuard<'static, Line>, String> {
    LINE.lock().map_err(|e| e.to_string())
}

fn webhook(request: &HttpRequest) -> HttpResponse {
    let Some(secret) = host::secret(SECRET) else {
        return respond(503, "text/plain", "LINE_CHANNEL_SECRET is not set");
    };
    let expected = sign::base64(&sign::hmac_sha256(secret.as_bytes(), &request.body));
    let given = adapter_kit::header(request, "x-line-signature").unwrap_or_default();
    if !sign::same(expected.as_bytes(), given.as_bytes()) {
        return respond(401, "text/plain", "bad signature");
    }
    let body: Value = serde_json::from_slice(&request.body).unwrap_or_default();
    let Ok(mut line) = lock() else {
        return respond(503, "text/plain", "busy");
    };
    for event in body["events"].as_array().into_iter().flatten() {
        line.event(event);
    }
    respond(200, "application/json", "{}")
}

fn media(path: &str) -> HttpResponse {
    let token = path
        .trim_start_matches("/media/")
        .split('/')
        .next()
        .unwrap_or_default();
    let Ok(line) = lock() else {
        return respond(503, "text/plain", "busy");
    };
    match line.served.iter().find(|(t, ..)| t == token) {
        Some((_, _, mime, bytes)) => respond(200, mime, bytes.clone()),
        None => respond(404, "text/plain", "gone"),
    }
}

struct Adapter;

impl Guest for Adapter {
    fn descriptor() -> String {
        json!({
            "platform": "line",
            "max_message_length": 5000,
            "length_unit": "chars",
            "splits_long_messages": true,
            "supports_edit": false,
            "supports_delete": false,
            "supports_typing": false,
            "supports_reactions": false,
            "supports_threads": false,
            "interactive": "numbered",
            "media_in": ["image", "audio", "video", "document"],
            "media_out": ["image", "audio", "video"],
            "delivery_receipts": false,
            "read_receipts": false,
            "streaming_tts": false,
            "markup": "plain",
            "binds_http": true,
            "enforces_own_access_policy": "allowlist",
        })
        .to_string()
    }

    fn start(options: String) -> Result<(), String> {
        let options: Value = serde_json::from_str(&options).map_err(|e| e.to_string())?;
        let public_url = options["public_url"]
            .as_str()
            .filter(|u| u.starts_with("https://"))
            .ok_or("options.public_url (https) is required: LINE fetches sent media from it")?
            .trim_end_matches('/')
            .to_owned();
        for name in [SECRET, TOKEN] {
            if host::secret(name).is_none() {
                return Err(format!("secret {name} is not set"));
            }
        }
        let base = |key: &str, default: &str| {
            options[key]
                .as_str()
                .unwrap_or(default)
                .trim_end_matches('/')
                .to_owned()
        };
        let settings = Settings {
            api: base("api_base", "https://api.line.me"),
            data_api: base("data_api_base", "https://api-data.line.me"),
            public_url,
            slow_secs: options["slow_response_secs"].as_u64().unwrap_or(45),
            users: misprefixed(&options, "allowed_users", 'U')?,
            groups: misprefixed(&options, "allowed_groups", 'C')?,
            rooms: misprefixed(&options, "allowed_rooms", 'R')?,
        };
        let mut line = lock()?;
        *line = Line::default();
        line.settings = Some(settings);
        line.epoch = next_epoch();
        Ok(())
    }

    fn stop() {
        if let Ok(mut line) = lock() {
            line.settings = None;
        }
    }

    fn poll() -> Result<String, String> {
        clock::tick();
        let mut line = lock()?;
        line.slow_turns()?;
        let inbound: Vec<Inbound> = line.inbox.drain(..).collect();
        Ok(message::batch(&inbound))
    }

    fn send(message: String) -> Result<String, String> {
        let out = Outgoing::parse(&message)?;
        let mut line = lock()?;
        let mut messages = bubbles(&adapter_kit::text::strip_markdown(&out.text));
        for file in &out.attachments {
            messages.push(line.file_message(file)?);
        }
        if let Some(pending) = line.pending(&out.chat)
            && pending.waiting
        {
            pending.held.extend(messages);
            return Ok(format!("held:{}", out.chat));
        }
        line.deliver(&out.chat, messages)
    }

    fn chat_info(chat: String) -> Result<String, String> {
        let kind = if chat.starts_with('U') { "dm" } else { "group" };
        Ok(message::chat_info(&chat, kind))
    }

    fn handle_http(request: HttpRequest) -> HttpResponse {
        let path = request
            .path
            .split('?')
            .next()
            .unwrap_or_default()
            .to_owned();
        match request.method.as_str() {
            "GET" if path.starts_with("/media/") => media(&path),
            "POST" => webhook(&request),
            _ => respond(405, "text/plain", ""),
        }
    }

    fn authorize_sender(inbound: String) -> String {
        let inbound: Value = serde_json::from_str(&inbound).unwrap_or_default();
        let (sender, chat) = (
            inbound["sender"].as_str().unwrap_or_default(),
            inbound["chat"].as_str().unwrap_or_default(),
        );
        let Ok(line) = lock() else {
            return "unknown".to_owned();
        };
        let Ok(s) = line.settings() else {
            return "unknown".to_owned();
        };
        let listed = s.users.iter().any(|u| u == sender)
            || s.groups.iter().chain(&s.rooms).any(|c| c == chat);
        if listed { "allow" } else { "unknown" }.to_owned()
    }
}

adapter_kit::bindings::export_adapter!(Adapter);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlists_take_ids_of_their_kind() {
        let options = json!({"allowed_users": ["Uabc"], "allowed_groups": ["Uoops"]});
        assert_eq!(
            misprefixed(&options, "allowed_users", 'U').unwrap(),
            ["Uabc"]
        );
        assert!(
            misprefixed(&options, "allowed_groups", 'C')
                .unwrap_err()
                .contains("starting with C")
        );
    }

    #[test]
    fn long_text_becomes_bubbles_of_4500() {
        let parts = bubbles(&"a ".repeat(5000));
        assert_eq!(parts.len(), 3);
        assert!(
            parts
                .iter()
                .all(|b| b["text"].as_str().unwrap().chars().count() <= BUBBLE_CHARS)
        );
    }
}

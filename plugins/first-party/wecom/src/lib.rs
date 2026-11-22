//! The WeCom smart robot adapter (spec: platform-adapters, WeCom smart
//! robot adapter): the AI Bot WebSocket (held by the socket bridge),
//! subscribed with the bot id and secret and pinged every 30 s.
//!
//! Replies are native `stream` messages answering the chat's last
//! callback: the first part opens a stream, edits update it in place, the
//! accumulated text is sent again every `options.stream_keepalive_secs`
//! (default 120) while nothing new arrives, and a stream older than 330 s
//! is finished so later parts go out as proactive messages. Inbound media
//! is downloaded and decrypted with its message's AES-256-CBC key;
//! outbound media is uploaded in 512 KB chunks within the per-kind limits
//! (image 10 MB, voice 2 MB, video 10 MB, file 20 MB): an oversized image,
//! voice or video goes as a file with a notice, anything over 20 MB is
//! refused with a notice.
//!
//! Access: `dm_policy` (open by default), `allow_from`, `group_policy`,
//! `group_allow_from` and per-group sender allowlists
//! `groups.<id>.allow_from` (`*` for any group).

use std::sync::Mutex;

use adapter_kit::bridge::Bridge;
use adapter_kit::message::{self, InFile, Inbound, OutFile, Outgoing};
use adapter_kit::policy::{self, Policy, Verdict};
use adapter_kit::{Guest, HttpRequest, HttpResponse, clock, crypt, host, net, respond, sign};
use serde_json::{Value, json};

mod upload;

const SECRET: &str = "WECOM_BOT_SECRET";
const MAX_CHARS: usize = 4000;
const PING_SECS: u64 = 30;
/// A stream older than this is finished; later parts go proactive.
const STREAM_SAFE_SECS: u64 = 330;
const MAX_BACKOFF_SECS: u64 = 60;
const SEEN: usize = 1000;
/// Frames waited for an answer to one request (each a bridge round trip).
const ANSWER_ROUNDS: usize = 4000;

/// An open stream reply.
struct Stream {
    chat: String,
    req_id: String,
    id: String,
    content: String,
    opened_ms: u64,
    sent_ms: u64,
}

struct Access {
    dm: Policy,
    allow_from: Vec<String>,
    group: Policy,
    group_allow_from: Vec<String>,
    groups: Value,
}

struct WeCom {
    url: String,
    bot_id: String,
    keepalive_secs: u64,
    access: Access,
    bridge: Bridge,
    connection: Option<String>,
    subscribe_id: Option<String>,
    ping_ms: u64,
    backoff_secs: u64,
    retry_at_ms: u64,
    seq: u64,
    /// chat -> its last callback's req_id.
    req_ids: Vec<(String, String)>,
    streams: Vec<Stream>,
    seen: Vec<String>,
    /// Frames read while waiting for an answer, for the next poll.
    held: Vec<String>,
    fatal: Option<String>,
}

static WC: Mutex<Option<WeCom>> = Mutex::new(None);

impl Access {
    fn of(options: &Value) -> Result<Self, String> {
        Ok(Self {
            dm: Policy::option(options, "dm_policy", Policy::Open)?,
            allow_from: policy::strings(options, "allow_from"),
            group: Policy::option(options, "group_policy", Policy::Open)?,
            group_allow_from: policy::strings(options, "group_allow_from"),
            groups: options["groups"].clone(),
        })
    }

    /// The verdict on `sender` writing in `chat`.
    fn verdict(&self, group: bool, chat: &str, sender: &str) -> Verdict {
        if !group {
            return self.dm.verdict(&self.allow_from, sender);
        }
        let chat_verdict = self.group.verdict(&self.group_allow_from, chat);
        if chat_verdict == Verdict::Deny {
            return Verdict::Deny;
        }
        let rule = self
            .groups
            .get(chat)
            .or_else(|| self.groups.get("*"))
            .map(|g| policy::strings(g, "allow_from"))
            .filter(|list| !list.is_empty());
        match rule {
            Some(list) if list.iter().any(|a| a == sender || a == "*") => Verdict::Allow,
            Some(_) => Verdict::Deny,
            None => chat_verdict,
        }
    }
}

/// The 32-byte key of a media `aeskey` (base64 WeCom sends unpadded).
fn media_key(aeskey: &str) -> Option<Vec<u8>> {
    let padded = format!("{aeskey}{}", "=".repeat((4 - aeskey.len() % 4) % 4));
    sign::base64_decode(&padded).ok().filter(|k| k.len() == 32)
}

/// Downloads and decrypts one inbound file.
fn fetch(kind: &'static str, media: &Value, fallback: &str) -> Option<InFile> {
    let url = media["url"].as_str()?;
    let response = net::send("GET", url, &[], None).ok()?;
    net::classify(&response).ok()?;
    let mut bytes = response.body;
    if let Some(key) = media["aeskey"].as_str().and_then(media_key) {
        let opened = crypt::aes256_cbc_decrypt(&key, &key[..16], &bytes).ok()?;
        bytes = crypt::unpad(opened, 32).ok()?;
    }
    let name = media["filename"].as_str().unwrap_or(fallback);
    Some(InFile::bytes(kind, name, bytes))
}

fn text_of(body: &Value) -> String {
    let content = |item: &Value, key: &str| {
        item[key]["content"]
            .as_str()
            .unwrap_or("")
            .trim()
            .to_owned()
    };
    match body["msgtype"].as_str().unwrap_or_default() {
        "mixed" => body["mixed"]["msg_item"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|i| i["msgtype"] == "text")
            .map(|i| content(i, "text"))
            .collect::<Vec<_>>()
            .join("\n"),
        // WeCom transcribes voice messages itself.
        "voice" => content(body, "voice"),
        _ => content(body, "text"),
    }
}

fn files_of(body: &Value) -> Vec<InFile> {
    let mut items = vec![body.clone()];
    if body["msgtype"] == "mixed" {
        items = body["mixed"]["msg_item"]
            .as_array()
            .cloned()
            .unwrap_or_default();
    }
    items
        .iter()
        .filter_map(|item| match item["msgtype"].as_str()? {
            "image" => fetch("image", &item["image"], "image.jpg"),
            "file" => fetch("document", &item["file"], "file"),
            "video" => fetch("video", &item["video"], "video.mp4"),
            _ => None,
        })
        .collect()
}

impl WeCom {
    fn frame(&self, cmd: &str, req_id: &str, body: &Value) -> Result<(), String> {
        let id = self.connection.clone().ok_or("server: not connected")?;
        let frame = json!({"cmd": cmd, "headers": {"req_id": req_id}, "body": body});
        self.bridge.send(&id, &frame.to_string())
    }

    fn new_req_id(&mut self, prefix: &str) -> String {
        self.seq += 1;
        format!("{prefix}-{}-{}", self.seq, clock::now_ms())
    }

    fn connect(&mut self) -> Result<(), String> {
        let secret =
            host::secret(SECRET).ok_or_else(|| format!("permanent: secret {SECRET} is not set"))?;
        let id = self.bridge.open(&self.url, &[])?;
        self.connection = Some(id);
        let req_id = self.new_req_id("subscribe");
        self.frame(
            "aibot_subscribe",
            &req_id,
            &json!({"bot_id": self.bot_id, "secret": secret}),
        )?;
        self.subscribe_id = Some(req_id);
        self.ping_ms = clock::now_ms();
        Ok(())
    }

    fn lost(&mut self) {
        if let Some(id) = self.connection.take() {
            self.bridge.close(&id);
        }
        self.streams.clear();
        self.req_ids.clear();
        self.retry_at_ms = clock::now_ms() + self.backoff_secs * 1000;
        self.backoff_secs = (self.backoff_secs * 2).min(MAX_BACKOFF_SECS);
    }

    /// Sends a request and waits for the frame answering it (others are
    /// held for the next poll): its body.
    fn request(&mut self, cmd: &str, body: &Value) -> Result<Value, String> {
        let req_id = self.new_req_id(cmd);
        self.frame(cmd, &req_id, body)?;
        self.answer(&req_id)
    }

    fn answer(&mut self, req_id: &str) -> Result<Value, String> {
        let id = self.connection.clone().ok_or("server: not connected")?;
        for _ in 0..ANSWER_ROUNDS {
            let received = self.bridge.recv(&id)?;
            let mut answer = None;
            for frame in received.frames {
                let value: Value = serde_json::from_str(&frame).unwrap_or_default();
                let callback = matches!(
                    value["cmd"].as_str(),
                    Some("aibot_msg_callback" | "aibot_callback" | "aibot_event_callback")
                );
                if answer.is_none() && value["headers"]["req_id"] == req_id && !callback {
                    answer = Some(value);
                } else {
                    self.held.push(frame);
                }
            }
            if let Some(value) = answer {
                return checked(&value);
            }
            if !received.open {
                return Err("server: the WeCom connection closed".to_owned());
            }
        }
        Err(format!("server: WeCom did not answer {req_id}"))
    }

    fn callback(&mut self, payload: &Value) -> Option<Inbound> {
        let body = &payload["body"];
        let req_id = payload["headers"]["req_id"].as_str().unwrap_or_default();
        let id = body["msgid"].as_str().unwrap_or(req_id).to_owned();
        if id.is_empty() || self.seen.contains(&id) {
            return None;
        }
        self.seen.push(id.clone());
        if self.seen.len() > SEEN {
            self.seen.remove(0);
        }
        let sender = body["from"]["userid"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let group = body["chattype"] == "group";
        let chat = body["chatid"]
            .as_str()
            .filter(|c| !c.is_empty())
            .unwrap_or(&sender)
            .to_owned();
        if chat.is_empty() || self.access.verdict(group, &chat, &sender) == Verdict::Deny {
            return None;
        }
        self.req_ids.retain(|(c, _)| *c != chat);
        self.req_ids.push((chat.clone(), req_id.to_owned()));
        let _ = self.finish_stream(&chat);
        let mut text = text_of(body);
        if group {
            // "@Bot /approve" -> "/approve".
            if let Some(rest) = text.strip_prefix('@') {
                text = rest
                    .split_once(char::is_whitespace)
                    .map_or("", |(_, r)| r)
                    .trim()
                    .to_owned();
            }
        }
        let files = files_of(body);
        if text.is_empty() && files.is_empty() {
            return None;
        }
        let mut inbound = Inbound::text(
            &id,
            &chat,
            if group { "group" } else { "dm" },
            &sender,
            &text,
        );
        inbound.mentioned = group;
        inbound.attachments = files;
        Some(inbound)
    }

    fn handle(&mut self, frame: &str, inbound: &mut Vec<Inbound>) {
        let Ok(payload) = serde_json::from_str::<Value>(frame) else {
            return;
        };
        let req_id = payload["headers"]["req_id"].as_str().unwrap_or_default();
        if self.subscribe_id.as_deref() == Some(req_id) {
            self.subscribe_id = None;
            if let Err(why) = checked(&payload) {
                self.fatal = Some(format!("fatal: wecom_auth_failed: {why}"));
            } else {
                self.backoff_secs = 2;
            }
            return;
        }
        if matches!(
            payload["cmd"].as_str(),
            Some("aibot_msg_callback" | "aibot_callback")
        ) {
            inbound.extend(self.callback(&payload));
        }
    }

    fn stream_frame(&mut self, at: usize, finish: bool) -> Result<(), String> {
        let stream = &self.streams[at];
        let body = json!({
            "msgtype": "stream",
            "stream": {"id": stream.id, "finish": finish, "content": stream.content},
        });
        let req_id = stream.req_id.clone();
        self.streams[at].sent_ms = clock::now_ms();
        // Answered under the callback's req_id: wait for it, so a refusal
        // is this send's and no stale answer is left for the next one.
        self.frame("aibot_respond_msg", &req_id, &body)?;
        self.answer(&req_id).map(|_| ())
    }

    /// Finishes `chat`'s open stream, if any (it is forgotten either way).
    fn finish_stream(&mut self, chat: &str) -> Result<(), String> {
        let Some(at) = self.streams.iter().position(|s| s.chat == chat) else {
            return Ok(());
        };
        let sent = self.stream_frame(at, true);
        self.streams.remove(at);
        sent
    }

    /// Keep-alives and the 330 s cut-over, once per poll.
    fn tend_streams(&mut self) {
        let mut at = 0;
        while at < self.streams.len() {
            let stream = &self.streams[at];
            if clock::elapsed(stream.opened_ms, STREAM_SAFE_SECS) {
                let chat = stream.chat.clone();
                let _ = self.finish_stream(&chat);
                continue;
            }
            if clock::elapsed(stream.sent_ms, self.keepalive_secs) {
                let _ = self.stream_frame(at, false);
            }
            at += 1;
        }
    }

    fn reply(&mut self, chat: &str, text: &str) -> Result<String, String> {
        // Finishing the previous stream is this send's first frame: a
        // throttled or busy platform fails the send (the gateway retries
        // it; the stream is gone, so the retry opens a fresh one), while
        // a refused finish (an expired stream) does not hold the reply.
        if let Err(why) = self.finish_stream(chat)
            && (why == "rate_limited" || why.starts_with("server:"))
        {
            return Err(why);
        }
        let content: String = text.chars().take(MAX_CHARS).collect();
        let Some(req_id) = self
            .req_ids
            .iter()
            .find(|(c, _)| c == chat)
            .map(|(_, r)| r.clone())
        else {
            return self.proactive(chat, &content);
        };
        let id = self.new_req_id("stream");
        self.streams.push(Stream {
            chat: chat.to_owned(),
            req_id,
            id: id.clone(),
            content,
            opened_ms: clock::now_ms(),
            sent_ms: 0,
        });
        self.stream_frame(self.streams.len() - 1, false)?;
        Ok(id)
    }

    fn proactive(&mut self, chat: &str, content: &str) -> Result<String, String> {
        let body = json!({"chatid": chat, "msgtype": "markdown", "markdown": {"content": content}});
        self.request("aibot_send_msg", &body)?;
        Ok(self.new_req_id("sent"))
    }

    fn edit(&mut self, out: &Outgoing) -> Result<String, String> {
        let id = out.message_id.clone().unwrap_or_default();
        let at = self
            .streams
            .iter()
            .position(|s| s.id == id)
            .ok_or("unsupported")?;
        self.streams[at].content = out.text.chars().take(MAX_CHARS).collect();
        self.stream_frame(at, false)?;
        Ok(id)
    }

    fn send(&mut self, out: &Outgoing) -> Result<String, String> {
        match out.op.as_str() {
            "" => {}
            "edit" => return self.edit(out),
            _ => return Err("unsupported".to_owned()),
        }
        let mut last = None;
        for file in &out.attachments {
            last = Some(self.send_file(&out.chat, file)?);
        }
        if out.text.trim().is_empty() {
            return last.ok_or_else(|| "permanent: nothing to send".to_owned());
        }
        self.reply(&out.chat, &out.text)
    }

    fn send_file(&mut self, chat: &str, file: &OutFile) -> Result<String, String> {
        let plan = upload::plan(file);
        if let Some(notice) = &plan.notice {
            self.proactive(chat, notice)?;
        }
        let Some(kind) = plan.kind else {
            return Ok(self.new_req_id("refused"));
        };
        let media_id = upload::upload(self, kind, file)?;
        let body = json!({"chatid": chat, "msgtype": kind, kind: {"media_id": media_id}});
        self.request("aibot_send_msg", &body)?;
        Ok(media_id)
    }
}

/// The body of an answer frame, or why it failed.
fn checked(frame: &Value) -> Result<Value, String> {
    let code = frame["errcode"]
        .as_i64()
        .or_else(|| frame["body"]["errcode"].as_i64())
        .unwrap_or(0);
    match code {
        0 => Ok(frame["body"].clone()),
        45009 => Err("rate_limited".to_owned()),
        -1 => Err("server: WeCom is busy".to_owned()),
        _ => Err(format!(
            "permanent: WeCom error {code}: {}",
            frame["errmsg"]
        )),
    }
}

fn poll_socket(wc: &mut WeCom) -> Result<Vec<Inbound>, String> {
    if let Some(why) = wc.fatal.clone() {
        return Err(why);
    }
    let Some(id) = wc.connection.clone() else {
        if clock::now_ms() >= wc.retry_at_ms && wc.connect().is_err() {
            wc.lost();
        }
        return Ok(Vec::new());
    };
    let mut inbound = Vec::new();
    for frame in std::mem::take(&mut wc.held) {
        wc.handle(&frame, &mut inbound);
    }
    let received = wc.bridge.recv(&id)?;
    for frame in &received.frames {
        wc.handle(frame, &mut inbound);
    }
    if !received.open {
        wc.lost();
        return Ok(inbound);
    }
    if clock::elapsed(wc.ping_ms, PING_SECS) {
        wc.ping_ms = clock::now_ms();
        let req_id = wc.new_req_id("ping");
        let _ = wc.frame("ping", &req_id, &json!({}));
    }
    wc.tend_streams();
    Ok(inbound)
}

fn with<T>(f: impl FnOnce(&mut WeCom) -> Result<T, String>) -> Result<T, String> {
    let mut slot = WC.lock().map_err(|e| e.to_string())?;
    f(slot.as_mut().ok_or("not started")?)
}

struct Adapter;

impl Guest for Adapter {
    fn descriptor() -> String {
        json!({
            "platform": "wecom",
            "max_message_length": MAX_CHARS,
            "length_unit": "chars",
            "splits_long_messages": false,
            "supports_edit": true,
            "supports_delete": false,
            "supports_typing": false,
            "supports_reactions": false,
            "supports_threads": false,
            "interactive": "numbered",
            "media_in": ["image", "video", "document"],
            "media_out": ["image", "voice", "video", "document"],
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
        let bot_id = options["bot_id"]
            .as_str()
            .filter(|b| !b.is_empty())
            .ok_or("options.bot_id is required")?
            .to_owned();
        let mut wc = WeCom {
            url: options["websocket_url"]
                .as_str()
                .unwrap_or("wss://openws.work.weixin.qq.com")
                .to_owned(),
            bot_id,
            keepalive_secs: options["stream_keepalive_secs"]
                .as_u64()
                .unwrap_or(120)
                .max(1),
            access: Access::of(&options)?,
            bridge: Bridge::from_options(&options)?,
            connection: None,
            subscribe_id: None,
            ping_ms: 0,
            backoff_secs: 2,
            retry_at_ms: 0,
            seq: 0,
            req_ids: Vec::new(),
            streams: Vec::new(),
            seen: Vec::new(),
            held: Vec::new(),
            fatal: None,
        };
        wc.connect()
            .map_err(|e| e.trim_start_matches("permanent: ").to_owned())?;
        let mut slot = WC.lock().map_err(|e| e.to_string())?;
        if let Some(mut old) = slot.take() {
            old.lost();
        }
        *slot = Some(wc);
        Ok(())
    }

    fn stop() {
        if let Ok(mut slot) = WC.lock()
            && let Some(mut wc) = slot.take()
        {
            let chats: Vec<String> = wc.streams.iter().map(|s| s.chat.clone()).collect();
            for chat in chats {
                let _ = wc.finish_stream(&chat);
            }
            wc.lost();
        }
    }

    fn poll() -> Result<String, String> {
        clock::tick();
        with(poll_socket).map(|inbound| message::batch(&inbound))
    }

    fn send(message: String) -> Result<String, String> {
        let out = Outgoing::parse(&message)?;
        with(|wc| wc.send(&out))
    }

    fn chat_info(chat: String) -> Result<String, String> {
        Ok(message::chat_info(&chat, "dm"))
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
        let verdict = WC.lock().ok().and_then(|slot| {
            slot.as_ref()
                .map(|wc| wc.access.verdict(group, chat, sender))
        });
        verdict.unwrap_or(Verdict::Unknown).as_str().to_owned()
    }
}

adapter_kit::bindings::export_adapter!(Adapter);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_group_sender_allowlist_narrows_the_group_policy() {
        let access = Access::of(&json!({"groups": {"G1": {"allow_from": ["alice"]}}})).unwrap();
        assert_eq!(access.verdict(true, "G1", "alice"), Verdict::Allow);
        assert_eq!(access.verdict(true, "G1", "bob"), Verdict::Deny);
        assert_eq!(access.verdict(true, "G2", "bob"), Verdict::Allow);
        assert_eq!(access.verdict(false, "bob", "bob"), Verdict::Allow);
    }

    #[test]
    fn media_keys_are_unpadded_base64() {
        let key = sign::base64(&[5u8; 32]);
        assert_eq!(media_key(key.trim_end_matches('=')).unwrap(), vec![5u8; 32]);
    }
}

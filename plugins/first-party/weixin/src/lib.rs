//! The Weixin adapter (spec: platform-adapters, Weixin personal WeChat
//! adapter): Tencent's iLink bot API for a personal WeChat account.
//!
//! Inbound is a 35-second `getupdates` long poll, run by the socket
//! bridge as one request so the adapter never blocks on it; its sync
//! cursor (`get_updates_buf`) is kept in profile state, so a restart
//! resumes where it stopped. Every reply must echo the peer's latest
//! `context_token`, also kept in profile state per peer (a cron message
//! after a restart still carries it); a send refused as a stale session
//! is retried once without it. CDN media is AES-128-ECB both ways. On
//! session expiry (`-14`) the adapter pauses 10 minutes. DMs default to
//! `open`, groups to `disabled` (with a WARN when enabled). The account id
//! is the credential the gateway locks across homes.

use std::sync::Mutex;

use adapter_kit::bridge::Bridge;
use adapter_kit::message::{self, Inbound, Outgoing};
use adapter_kit::policy::{self, Policy, Verdict};
use adapter_kit::{Guest, HttpRequest, HttpResponse, clock, host, net, respond, sign};
use serde_json::{Map, Value, json};

mod media;

const TOKEN: &str = "WEIXIN_TOKEN";
const MAX_CHARS: usize = 4000;
const SESSION_EXPIRED: i64 = -14;
const STALE: i64 = -2;
const PAUSE_SECS: u64 = 600;
const LONG_POLL_MS: u64 = 35_000;
const SEEN: usize = 1000;
const CURSOR_KEY: &str = "sync";
const TOKENS_KEY: &str = "context_tokens";

struct Weixin {
    base: String,
    cdn: String,
    account: String,
    token: String,
    dm: Policy,
    allow_from: Vec<String>,
    group: Policy,
    group_allow_from: Vec<String>,
    bridge: Bridge,
    /// The long poll in flight (a bridge connection id).
    in_flight: Option<String>,
    paused_until_ms: u64,
    retry_at_ms: u64,
    cursor: String,
    contexts: Map<String, Value>,
    seen: Vec<String>,
    /// Groups the chats seen so far are in.
    groups: Vec<String>,
    seq: u64,
}

static WX: Mutex<Option<Weixin>> = Mutex::new(None);

/// Whether an answer says the session expired.
fn expired(answer: &Value) -> bool {
    [&answer["ret"], &answer["errcode"]]
        .iter()
        .any(|v| v.as_i64() == Some(SESSION_EXPIRED))
}

/// Whether a send was refused because the context token went stale.
fn stale(answer: &Value) -> bool {
    let code = |v: &Value| v.as_i64() == Some(STALE);
    let message = answer["errmsg"].as_str().unwrap_or_default().to_lowercase();
    (code(&answer["ret"]) || code(&answer["errcode"]))
        && matches!(message.as_str(), "unknown error" | "prepare failed")
}

fn failed(answer: &Value) -> bool {
    [&answer["ret"], &answer["errcode"]]
        .iter()
        .any(|v| v.as_i64().is_some_and(|c| c != 0))
}

impl Weixin {
    fn headers(&self) -> Vec<(String, String)> {
        // A per-request client tag WeChat expects; any number will do.
        let uin = sign::base64((clock::now_ms() % 4_294_967_296).to_string().as_bytes());
        vec![
            ("content-type".into(), "application/json".into()),
            ("authorizationtype".into(), "ilink_bot_token".into()),
            ("authorization".into(), format!("Bearer {}", self.token)),
            ("x-wechat-uin".into(), uin),
            ("ilink-app-id".into(), "bot".into()),
        ]
    }

    fn post(&self, endpoint: &str, body: &Value) -> Result<Value, String> {
        let headers = self.headers();
        let pairs: Vec<(&str, &str)> = headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let response = net::send(
            "POST",
            &format!("{}/{endpoint}", self.base),
            &pairs,
            Some(body.to_string().into_bytes()),
        )?;
        net::classify(&response)?;
        Ok(response.json())
    }

    fn start_poll(&mut self) -> Result<(), String> {
        let headers = self.headers();
        let pairs: Vec<(&str, &str)> = headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let body = json!({"get_updates_buf": self.cursor, "longpolling_timeout_ms": LONG_POLL_MS});
        let id = self.bridge.request(
            "POST",
            &format!("{}/ilink/bot/getupdates", self.base),
            &pairs,
            &body.to_string(),
        )?;
        self.in_flight = Some(id);
        Ok(())
    }

    /// The chat and type of one message.
    fn chat_of(&self, message: &Value) -> (String, bool) {
        let room = message["room_id"]
            .as_str()
            .or(message["group_id"].as_str())
            .unwrap_or_default();
        if room.is_empty() {
            (
                message["from_user_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                false,
            )
        } else {
            (room.to_owned(), true)
        }
    }

    fn admitted(&self, group: bool, chat: &str, sender: &str) -> bool {
        let verdict = if group {
            self.group.verdict(&self.group_allow_from, chat)
        } else {
            self.dm.verdict(&self.allow_from, sender)
        };
        verdict != Verdict::Deny
    }

    fn remember_context(&mut self, peer: &str, token: &str) {
        if token.is_empty() || self.contexts.get(peer).and_then(Value::as_str) == Some(token) {
            return;
        }
        self.contexts.insert(peer.to_owned(), json!(token));
        host::store(TOKENS_KEY, &Value::Object(self.contexts.clone()));
    }

    fn message(&mut self, message: &Value) -> Option<Inbound> {
        // Messages the bot sent itself come back too.
        if message["message_type"].as_i64() == Some(2) {
            return None;
        }
        let sender = message["from_user_id"].as_str()?.to_owned();
        if sender == self.account {
            return None;
        }
        let (chat, group) = self.chat_of(message);
        let id = message["message_id"]
            .as_str()
            .map(str::to_owned)
            .or_else(|| message["message_id"].as_u64().map(|n| n.to_string()))
            .or_else(|| message["seq"].as_u64().map(|n| format!("seq-{n}")))
            .unwrap_or_else(|| format!("{sender}-{}", message["create_time_ms"]));
        if self.seen.contains(&id) {
            return None;
        }
        self.seen.push(id.clone());
        if self.seen.len() > SEEN {
            self.seen.remove(0);
        }
        if let Some(token) = message["context_token"].as_str() {
            self.remember_context(&chat, token);
        }
        if !self.admitted(group, &chat, &sender) {
            return None;
        }
        if group && !self.groups.contains(&chat) {
            self.groups.push(chat.clone());
        }
        let items = message["item_list"].as_array().cloned().unwrap_or_default();
        let text = media::text_of(&items);
        let files = media::files_of(&self.cdn, &items);
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
        inbound.attachments = files;
        Some(inbound)
    }

    fn updates(&mut self, frame: &str) -> Result<Vec<Inbound>, String> {
        let outer: Value = serde_json::from_str(frame).unwrap_or_default();
        let answer: Value =
            serde_json::from_str(outer["body"].as_str().unwrap_or_default()).unwrap_or_default();
        let status = outer["status"].as_u64().unwrap_or(0);
        if status == 401 || status == 403 {
            return Err(format!(
                "fatal: weixin_unauthorized: iLink answered {status}"
            ));
        }
        if expired(&answer) {
            host::warn("Weixin session expired; pausing 10 minutes before polling again");
            self.paused_until_ms = clock::now_ms() + PAUSE_SECS * 1000;
            return Ok(Vec::new());
        }
        if status != 200 || failed(&answer) {
            self.retry_at_ms = clock::now_ms() + 2000;
            return Ok(Vec::new());
        }
        if let Some(cursor) = answer["get_updates_buf"].as_str().filter(|c| !c.is_empty()) {
            cursor.clone_into(&mut self.cursor);
            host::store(CURSOR_KEY, &json!(cursor));
        }
        let messages = answer["msgs"].as_array().cloned().unwrap_or_default();
        Ok(messages.iter().filter_map(|m| self.message(m)).collect())
    }

    fn poll(&mut self) -> Result<Vec<Inbound>, String> {
        let now = clock::now_ms();
        if now < self.paused_until_ms || now < self.retry_at_ms {
            return Ok(Vec::new());
        }
        let Some(id) = self.in_flight.clone() else {
            if self.start_poll().is_err() {
                self.retry_at_ms = now + 2000;
            }
            return Ok(Vec::new());
        };
        let received = self.bridge.recv(&id)?;
        let mut inbound = Vec::new();
        for frame in &received.frames {
            inbound.extend(self.updates(frame)?);
        }
        if !received.open {
            self.in_flight = None;
            if received.error.is_some() {
                self.retry_at_ms = now + 2000;
            }
        }
        Ok(inbound)
    }

    fn client_id(&mut self) -> String {
        self.seq += 1;
        format!("prometheus-{}-{}", clock::now_ms(), self.seq)
    }

    /// Sends `items` to `chat` with its context token, once more without
    /// it when the token went stale.
    fn send_items(&mut self, chat: &str, items: Vec<Value>) -> Result<String, String> {
        let client_id = self.client_id();
        let mut token = self
            .contexts
            .get(chat)
            .and_then(Value::as_str)
            .map(str::to_owned);
        loop {
            let mut message = json!({
                "from_user_id": "", "to_user_id": chat, "client_id": client_id,
                "message_type": 2, "message_state": 2, "item_list": items,
            });
            if let Some(token) = &token {
                message["context_token"] = json!(token);
            }
            let answer = self.post("ilink/bot/sendmessage", &json!({"msg": message}))?;
            if stale(&answer) && token.is_some() {
                token = None;
                continue;
            }
            if expired(&answer) {
                self.paused_until_ms = clock::now_ms() + PAUSE_SECS * 1000;
                return Err("server: the Weixin session expired".to_owned());
            }
            if stale(&answer) {
                return Err(
                    "permanent: iLink will not deliver until this peer messages the bot again"
                        .to_owned(),
                );
            }
            if failed(&answer) {
                return Err(format!("permanent: iLink refused the message: {answer}"));
            }
            return Ok(client_id);
        }
    }

    fn send(&mut self, out: &Outgoing) -> Result<String, String> {
        if !out.op.is_empty() {
            return Err("unsupported".to_owned());
        }
        let mut last = None;
        for file in &out.attachments {
            let item = media::upload(self, &out.chat, file)?;
            last = Some(self.send_items(&out.chat, vec![item])?);
        }
        if out.text.trim().is_empty() {
            return last.ok_or_else(|| "permanent: nothing to send".to_owned());
        }
        let text = adapter_kit::text::strip_markdown(&out.text);
        self.send_items(
            &out.chat,
            vec![json!({"type": 1, "text_item": {"text": text}})],
        )
    }
}

fn with<T>(f: impl FnOnce(&mut Weixin) -> Result<T, String>) -> Result<T, String> {
    let mut slot = WX.lock().map_err(|e| e.to_string())?;
    f(slot.as_mut().ok_or("not started")?)
}

struct Adapter;

impl Guest for Adapter {
    fn descriptor() -> String {
        json!({
            "platform": "weixin",
            "max_message_length": MAX_CHARS,
            "length_unit": "chars",
            "splits_long_messages": false,
            "supports_edit": false,
            "supports_delete": false,
            "supports_typing": false,
            "supports_reactions": false,
            "supports_threads": false,
            "interactive": "numbered",
            "media_in": ["image", "voice", "video", "document"],
            "media_out": ["image", "video", "document"],
            "delivery_receipts": false,
            "read_receipts": false,
            "streaming_tts": false,
            "markup": "plain",
            "binds_http": false,
            "enforces_own_access_policy": "allowlist",
            "credential_options": ["account_id"],
        })
        .to_string()
    }

    fn start(options: String) -> Result<(), String> {
        let options: Value = serde_json::from_str(&options).map_err(|e| e.to_string())?;
        let token = host::secret(TOKEN).ok_or(format!(
            "secret {TOKEN} is not set (log in with `prometheus setup gateway weixin`)"
        ))?;
        let account = options["account_id"]
            .as_str()
            .filter(|a| !a.is_empty())
            .ok_or("options.account_id is required (set by `prometheus setup gateway weixin`)")?
            .to_owned();
        let group = Policy::option(&options, "group_policy", Policy::Disabled)?;
        if group != Policy::Disabled {
            host::warn(
                "Weixin group chats are enabled: the bot answers in groups of a personal account",
            );
        }
        let base = |key: &str, default: &str| {
            options[key]
                .as_str()
                .unwrap_or(default)
                .trim_end_matches('/')
                .to_owned()
        };
        let wx = Weixin {
            base: base("base_url", "https://ilinkai.weixin.qq.com"),
            cdn: base("cdn_base_url", "https://novac2c.cdn.weixin.qq.com/c2c"),
            account,
            token,
            dm: Policy::option(&options, "dm_policy", Policy::Open)?,
            allow_from: policy::strings(&options, "allow_from"),
            group,
            group_allow_from: policy::strings(&options, "group_allow_from"),
            bridge: Bridge::from_options(&options)?,
            in_flight: None,
            paused_until_ms: 0,
            retry_at_ms: 0,
            cursor: host::load(CURSOR_KEY)
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default(),
            contexts: host::load(TOKENS_KEY)
                .and_then(|v| v.as_object().cloned())
                .unwrap_or_default(),
            seen: Vec::new(),
            groups: Vec::new(),
            seq: 0,
        };
        let mut slot = WX.lock().map_err(|e| e.to_string())?;
        if let Some(old) = slot.take()
            && let Some(id) = old.in_flight
        {
            old.bridge.close(&id);
        }
        *slot = Some(wx);
        Ok(())
    }

    fn stop() {
        if let Ok(mut slot) = WX.lock()
            && let Some(wx) = slot.take()
            && let Some(id) = wx.in_flight
        {
            wx.bridge.close(&id);
        }
    }

    fn poll() -> Result<String, String> {
        clock::tick();
        with(Weixin::poll).map(|inbound| message::batch(&inbound))
    }

    fn send(message: String) -> Result<String, String> {
        let out = Outgoing::parse(&message)?;
        with(|wx| wx.send(&out))
    }

    fn chat_info(chat: String) -> Result<String, String> {
        let group = with(|wx| Ok(wx.groups.contains(&chat)))?;
        Ok(message::chat_info(
            &chat,
            if group { "group" } else { "dm" },
        ))
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
        let verdict = WX.lock().ok().and_then(|slot| {
            slot.as_ref().map(|wx| {
                if group {
                    wx.group.verdict(&wx.group_allow_from, chat)
                } else {
                    wx.dm.verdict(&wx.allow_from, sender)
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
    fn stale_and_expired_answers_are_told_apart() {
        assert!(expired(&json!({"ret": -14})));
        assert!(stale(&json!({"ret": -2, "errmsg": "prepare failed"})));
        assert!(!stale(&json!({"ret": -2, "errmsg": "frequency limit"})));
        assert!(failed(&json!({"errcode": 5})) && !failed(&json!({"ret": 0})));
    }
}

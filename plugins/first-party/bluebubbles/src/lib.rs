//! The BlueBubbles iMessage adapter (spec: platform-adapters, BlueBubbles
//! iMessage adapter): the BlueBubbles server's REST API for sends, and its
//! webhooks for inbound. On connect the adapter registers its own webhook
//! for `new-message` and `updated-message` pointing at its shared-listener
//! route (`options.public_url`, with a token derived from the server
//! password in the query, since the server sends no signature) and removes
//! it on a clean shutdown.
//!
//! Chats are addressed by email or phone, resolved to chat GUIDs (a GUID
//! passes as it is; an unknown address starts a new DM). Text goes out
//! with Markdown stripped. When the server's Private API is on, tapbacks,
//! typing and read receipts (`options.send_read_receipts`, default true)
//! use it. In group chats with `options.require_mention`, only messages
//! matching `options.mention_patterns` are answered; DMs never are gated.

use std::sync::Mutex;

use adapter_kit::message::{self, InFile, Inbound, Outgoing};
use adapter_kit::{Guest, HttpRequest, HttpResponse, host, net, policy, respond, sign, text};
use serde_json::{Value, json};

const PASSWORD: &str = "BLUEBUBBLES_PASSWORD";
const EVENTS: [&str; 2] = ["new-message", "updated-message"];

struct BlueBubbles {
    server: String,
    webhook_url: String,
    webhook_id: Option<String>,
    private_api: bool,
    read_receipts: bool,
    require_mention: bool,
    patterns: Vec<String>,
    temp: u64,
}

static BB: Mutex<Option<BlueBubbles>> = Mutex::new(None);
static INBOX: Mutex<Vec<Inbound>> = Mutex::new(Vec::new());

fn password() -> Result<String, String> {
    host::secret(PASSWORD).ok_or_else(|| format!("permanent: secret {PASSWORD} is not set"))
}

/// The token the registered webhook URL carries: unguessable without the
/// server password, stable across restarts (a crash leaves no stale
/// registration that a new token would orphan).
fn webhook_token(password: &str) -> String {
    sign::hex(&sign::hmac_sha256(
        password.as_bytes(),
        b"prometheus-bluebubbles-webhook",
    ))
}

/// The tapback the server takes for an emoji: iMessage has six, so any
/// other emoji (an acknowledging 👀) shows as a like.
fn tapback(emoji: &str) -> &'static str {
    match emoji {
        "\u{2764}\u{fe0f}" | "\u{2764}" => "love",
        "\u{1f44d}" => "like",
        "\u{1f44e}" => "dislike",
        "\u{1f602}" | "\u{1f923}" => "laugh",
        "\u{203c}\u{fe0f}" | "\u{2757}" => "emphasize",
        "\u{2753}" => "question",
        _ => "like",
    }
}

impl BlueBubbles {
    fn url(&self, path: &str) -> Result<String, String> {
        let sep = if path.contains('?') { '&' } else { '?' };
        Ok(format!(
            "{}{path}{sep}password={}",
            self.server,
            text::percent_encode(&password()?)
        ))
    }

    fn post(&self, path: &str, body: &Value) -> Result<Value, String> {
        net::json("POST", &self.url(path)?, &[], body)
    }

    fn next_temp(&mut self) -> String {
        self.temp += 1;
        format!("temp-prometheus-{}", self.temp)
    }

    /// A chat GUID for an address (or a GUID); `None` when there is no
    /// chat with it yet.
    fn guid(&self, target: &str) -> Result<Option<String>, String> {
        if target.contains(';') {
            return Ok(Some(target.to_owned()));
        }
        let chats = self.post("/api/v1/chat/query", &json!({"limit": 1000, "offset": 0}))?;
        Ok(chats["data"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|c| c["chatIdentifier"] == target || c["identifier"] == target)
            .and_then(|c| c["guid"].as_str().map(str::to_owned)))
    }

    fn register(&mut self) -> Result<(), String> {
        let listed = net::get(&self.url("/api/v1/webhook")?, &[])?;
        let existing = listed["data"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|w| w["url"] == self.webhook_url.as_str());
        let id = match existing {
            Some(hook) => hook["id"].clone(),
            None => {
                let body = json!({"url": self.webhook_url, "events": EVENTS});
                self.post("/api/v1/webhook", &body)?["data"]["id"].clone()
            }
        };
        self.webhook_id = Some(match id {
            Value::String(s) => s,
            other => other.to_string(),
        });
        Ok(())
    }

    fn mark_read(&self, chat: &str) {
        if !(self.private_api && self.read_receipts) {
            return;
        }
        let path = format!("/api/v1/chat/{}/read", text::percent_encode(chat));
        if let Err(why) = self
            .url(&path)
            .and_then(|u| net::send("POST", &u, &[], None))
        {
            host::warn(&format!("BlueBubbles did not mark a chat read: {why}"));
        }
    }

    fn mentions(&self, body: &str) -> bool {
        let lower = body.to_lowercase();
        self.patterns
            .iter()
            .any(|p| lower.contains(&p.to_lowercase()))
    }

    fn attachments(&self, data: &Value) -> Vec<InFile> {
        let mut files = Vec::new();
        for item in data["attachments"].as_array().into_iter().flatten() {
            let (Some(guid), Some(name)) = (item["guid"].as_str(), item["transferName"].as_str())
            else {
                continue;
            };
            let path = format!("/api/v1/attachment/{}/download", text::percent_encode(guid));
            match self
                .url(&path)
                .and_then(|u| net::send("GET", &u, &[], None))
            {
                Ok(response) if response.status < 300 => {
                    files.push(InFile::bytes(
                        message::kind_of_name(name),
                        name,
                        response.body,
                    ));
                }
                _ => host::warn(&format!("could not download BlueBubbles attachment {guid}")),
            }
        }
        files
    }

    /// One webhook event: the message to admit, if any.
    fn event(&self, event: &Value) -> Option<Inbound> {
        if !EVENTS.contains(&event["type"].as_str()?) {
            return None;
        }
        let data = &event["data"];
        if data["isFromMe"] == true {
            return None;
        }
        // Tapbacks arrive as messages associated with another one.
        if data["associatedMessageGuid"]
            .as_str()
            .is_some_and(|g| !g.is_empty())
        {
            return None;
        }
        let chat = data["chats"][0]["guid"].as_str()?;
        let sender = data["handle"]["address"].as_str()?;
        let body = data["text"].as_str().unwrap_or_default();
        let group = chat.contains(";+;");
        if group && self.require_mention && !self.mentions(body) {
            return None;
        }
        let kind = if group { "group" } else { "dm" };
        let mut inbound = Inbound::text(data["guid"].as_str()?, chat, kind, sender, body);
        // In a group, only addressed messages got this far.
        inbound.mentioned = group;
        inbound.attachments = self.attachments(data);
        self.mark_read(chat);
        Some(inbound)
    }

    fn send_text(&mut self, out: &Outgoing) -> Result<String, String> {
        let plain = text::strip_markdown(&out.text);
        let temp = self.next_temp();
        let method = if self.private_api {
            "private-api"
        } else {
            "apple-script"
        };
        let answer = match self.guid(&out.chat)? {
            Some(guid) => {
                let mut body =
                    json!({"chatGuid": guid, "tempGuid": temp, "message": plain, "method": method});
                if let Some(reply) = out.reply_to.as_deref().filter(|_| self.private_api) {
                    body["selectedMessageGuid"] = json!(reply);
                }
                self.post("/api/v1/message/text", &body)?
            }
            None => self.post(
                "/api/v1/chat/new",
                &json!({"addresses": [out.chat], "message": plain, "tempGuid": temp}),
            )?,
        };
        Ok(answer["data"]["guid"].as_str().unwrap_or(&temp).to_owned())
    }

    fn send_file(&mut self, chat: &str, file: &message::OutFile) -> Result<String, String> {
        let guid = self
            .guid(chat)?
            .unwrap_or_else(|| format!("iMessage;-;{chat}"));
        let temp = self.next_temp();
        let (content_type, body) = net::multipart(
            &[
                ("chatGuid", &guid),
                ("tempGuid", &temp),
                ("name", &file.name),
            ],
            ("attachment", &file.name, &file.mime, &file.data),
        );
        let response = net::send(
            "POST",
            &self.url("/api/v1/message/attachment")?,
            &[("content-type", &content_type)],
            Some(body),
        )?;
        net::classify(&response)?;
        Ok(response.json()["data"]["guid"]
            .as_str()
            .unwrap_or(&temp)
            .to_owned())
    }

    fn react(&mut self, out: &Outgoing) -> Result<String, String> {
        let emoji = out.emoji.as_deref().unwrap_or_default();
        let name = tapback(emoji);
        let reaction = if out.remove {
            format!("-{name}")
        } else {
            name.to_owned()
        };
        let guid = self.guid(&out.chat)?.ok_or("permanent: no such chat")?;
        let body = json!({
            "chatGuid": guid,
            "selectedMessageGuid": out.message_id,
            "reaction": reaction,
        });
        self.post("/api/v1/message/react", &body)?;
        Ok(String::new())
    }

    fn typing(&self, chat: &str) -> Result<String, String> {
        let guid = self.guid(chat)?.ok_or("permanent: no such chat")?;
        let path = format!("/api/v1/chat/{}/typing", text::percent_encode(&guid));
        let response = net::send("POST", &self.url(&path)?, &[], None)?;
        net::classify(&response)?;
        Ok(String::new())
    }
}

fn with<T>(f: impl FnOnce(&mut BlueBubbles) -> Result<T, String>) -> Result<T, String> {
    let mut slot = BB.lock().map_err(|e| e.to_string())?;
    f(slot.as_mut().ok_or("not started")?)
}

fn webhook(request: &HttpRequest) -> HttpResponse {
    let Some(password) = host::secret(PASSWORD) else {
        return respond(503, "text/plain", "BLUEBUBBLES_PASSWORD is not set");
    };
    let token = adapter_kit::query(&request.path)
        .into_iter()
        .find(|(k, _)| k == "token")
        .map(|(_, v)| v)
        .unwrap_or_default();
    if !sign::same(token.as_bytes(), webhook_token(&password).as_bytes()) {
        return respond(401, "text/plain", "bad token");
    }
    let event: Value = serde_json::from_slice(&request.body).unwrap_or_default();
    let message = with(|bb| Ok(bb.event(&event))).ok().flatten();
    if let (Some(message), Ok(mut inbox)) = (message, INBOX.lock()) {
        inbox.push(message);
    }
    respond(200, "text/plain", "ok")
}

struct Adapter;

impl Guest for Adapter {
    fn descriptor() -> String {
        // Typing and tapbacks need the server's Private API: the gateway
        // gets `unsupported` from a server without it.
        json!({
            "platform": "bluebubbles",
            "max_message_length": 10000,
            "length_unit": "chars",
            "splits_long_messages": false,
            "supports_edit": false,
            "supports_delete": false,
            "supports_typing": true,
            "supports_reactions": true,
            "supports_threads": false,
            "interactive": "numbered",
            "media_in": ["image", "audio", "video", "document"],
            "media_out": ["image", "audio", "video", "document"],
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
        let required = |key: &str| {
            options[key]
                .as_str()
                .filter(|v| !v.is_empty())
                .map(|v| v.trim_end_matches('/').to_owned())
                .ok_or_else(|| format!("options.{key} is required"))
        };
        let (server, public_url) = (required("server_url")?, required("public_url")?);
        let password = password().map_err(|e| e.trim_start_matches("permanent: ").to_owned())?;
        let sep = if public_url.contains('?') { '&' } else { '?' };
        let mut bb = BlueBubbles {
            server,
            webhook_url: format!("{public_url}/{sep}token={}", webhook_token(&password)),
            webhook_id: None,
            private_api: false,
            read_receipts: options["send_read_receipts"].as_bool().unwrap_or(true),
            require_mention: options["require_mention"].as_bool().unwrap_or(false),
            patterns: policy::strings(&options, "mention_patterns"),
            temp: 0,
        };
        let info = net::get(&bb.url("/api/v1/server/info")?, &[])?;
        bb.private_api = info["data"]["private_api"] == true;
        bb.register()?;
        *BB.lock().map_err(|e| e.to_string())? = Some(bb);
        Ok(())
    }

    fn stop() {
        let Ok(mut slot) = BB.lock() else { return };
        let Some(bb) = slot.take() else { return };
        if let Some(id) = &bb.webhook_id {
            let path = format!("/api/v1/webhook/{}", text::percent_encode(id));
            if let Err(why) = bb
                .url(&path)
                .and_then(|u| net::send("DELETE", &u, &[], None))
            {
                host::warn(&format!("could not remove the BlueBubbles webhook: {why}"));
            }
        }
    }

    fn poll() -> Result<String, String> {
        let drained: Vec<Inbound> = INBOX
            .lock()
            .map(|mut i| i.drain(..).collect())
            .unwrap_or_default();
        Ok(message::batch(&drained))
    }

    fn send(message: String) -> Result<String, String> {
        let out = Outgoing::parse(&message)?;
        with(|bb| match out.op.as_str() {
            "" => {
                let mut id = String::new();
                if !out.text.is_empty() || out.attachments.is_empty() {
                    id = bb.send_text(&out)?;
                }
                for file in &out.attachments {
                    id = bb.send_file(&out.chat, file)?;
                }
                Ok(id)
            }
            "react" if bb.private_api => bb.react(&out),
            "typing" if bb.private_api => bb.typing(&out.chat),
            _ => Err("unsupported".to_owned()),
        })
    }

    fn chat_info(chat: String) -> Result<String, String> {
        let kind = if chat.contains(";+;") { "group" } else { "dm" };
        Ok(message::chat_info(&chat, kind))
    }

    fn handle_http(request: HttpRequest) -> HttpResponse {
        if request.method != "POST" {
            return respond(405, "text/plain", "POST only");
        }
        webhook(&request)
    }

    fn authorize_sender(_inbound: String) -> String {
        "unknown".to_owned()
    }
}

adapter_kit::bindings::export_adapter!(Adapter);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_webhook_token_depends_on_the_password() {
        assert_ne!(webhook_token("a"), webhook_token("b"));
        assert_eq!(webhook_token("a"), webhook_token("a"));
    }

    #[test]
    fn known_emoji_map_to_tapbacks() {
        assert_eq!(tapback("\u{1f602}"), "laugh");
        assert_eq!(tapback("\u{1f440}"), "like");
    }
}

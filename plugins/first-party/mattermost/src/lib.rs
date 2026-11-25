//! The Mattermost adapter (spec: platform-adapters, Mattermost adapter):
//! REST API v4 for sends and files, the `/api/v4/websocket` event stream
//! (held by the socket bridge) for inbound posts, reconnecting with a
//! backoff from 2 to 60 seconds.
//!
//! Gates, in order: `options.allowed_channels` (a post in any other
//! channel is dropped, mention or not; direct messages too when the list
//! is set), then DMs always answered, channel posts only when they
//! @mention the bot or sit in `options.free_response_channels`.
//! `options.reply_mode = "thread"` threads every reply under the post it
//! answers (`off`, the default, threads only inside an existing thread).
//! Users are 26-character Mattermost user ids; allowlist entries of any
//! other shape are refused at start. `/sethome` and the allowlists
//! themselves are the gateway's.

use std::sync::Mutex;

use adapter_kit::bridge::Bridge;
use adapter_kit::message::{self, InFile, Inbound, Outgoing};
use adapter_kit::{Guest, HttpRequest, HttpResponse, clock, host, net, policy, respond, text};
use serde_json::{Value, json};

const TOKEN: &str = "MATTERMOST_TOKEN";
const MAX_CHARS: usize = 16383;
/// Reconnect waits double from 2 s to this.
const MAX_BACKOFF_SECS: u64 = 60;

struct Mattermost {
    api: String,
    ws: String,
    bot_id: String,
    bot_name: String,
    allowed: Vec<String>,
    free: Vec<String>,
    thread_replies: bool,
    bridge: Bridge,
    connection: Option<String>,
    backoff_secs: u64,
    retry_at_ms: u64,
}

static MM: Mutex<Option<Mattermost>> = Mutex::new(None);

fn bearer() -> Result<String, String> {
    host::secret(TOKEN)
        .map(|t| format!("Bearer {t}"))
        .ok_or_else(|| format!("permanent: secret {TOKEN} is not set"))
}

/// A Mattermost user id: 26 lower-case letters and digits.
fn is_user_id(id: &str) -> bool {
    id.len() == 26
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

/// The allowlist entries of `options.access` that are not user ids.
fn bad_user_ids(options: &Value) -> Vec<String> {
    policy::strings(&options["access"], "allow_users")
        .into_iter()
        .filter(|id| !is_user_id(id))
        .collect()
}

/// `text` without `@name` (any case), and whether it was there.
fn strip_mention(text: &str, name: &str) -> (String, bool) {
    let needle = format!("@{}", name.to_lowercase());
    let lower = text.to_lowercase();
    match lower.find(&needle) {
        Some(at) => {
            let end = at + needle.len();
            let cleaned = format!("{}{}", &text[..at], &text[end..]);
            (
                cleaned.split_whitespace().collect::<Vec<_>>().join(" "),
                true,
            )
        }
        None => (text.to_owned(), false),
    }
}

impl Mattermost {
    fn get(&self, path: &str) -> Result<Value, String> {
        net::get(
            &format!("{}{path}", self.api),
            &[("authorization", &bearer()?)],
        )
    }

    fn connect(&mut self) -> Result<(), String> {
        let auth = bearer()?;
        let id = self.bridge.open(&self.ws, &[("authorization", &auth)])?;
        self.connection = Some(id);
        Ok(())
    }

    fn lost(&mut self) {
        if let Some(id) = self.connection.take() {
            self.bridge.close(&id);
        }
        self.retry_at_ms = clock::now_ms() + self.backoff_secs * 1000;
        self.backoff_secs = (self.backoff_secs * 2).min(MAX_BACKOFF_SECS);
    }

    /// The post's files, downloaded under the plugin's grant.
    fn files(&self, post: &Value) -> Vec<InFile> {
        let Ok(auth) = bearer() else {
            return Vec::new();
        };
        let mut files = Vec::new();
        for id in post["file_ids"].as_array().into_iter().flatten() {
            let Some(id) = id.as_str() else { continue };
            let info = self.get(&format!("/api/v4/files/{id}/info"));
            let name = info
                .as_ref()
                .ok()
                .and_then(|i| i["name"].as_str())
                .unwrap_or(id)
                .to_owned();
            let url = format!("{}/api/v4/files/{id}", self.api);
            match net::send("GET", &url, &[("authorization", &auth)], None) {
                Ok(response) if response.status < 300 => {
                    files.push(InFile::bytes(
                        message::kind_of_name(&name),
                        &name,
                        response.body,
                    ));
                }
                _ => host::warn(&format!("could not download Mattermost file {id}")),
            }
        }
        files
    }

    /// A `posted` event as an inbound message, after the gates.
    fn posted(&self, data: &Value) -> Option<Inbound> {
        let post: Value = serde_json::from_str(data["post"].as_str()?).ok()?;
        let (id, channel, user) = (
            post["id"].as_str()?,
            post["channel_id"].as_str()?,
            post["user_id"].as_str()?,
        );
        // System posts carry a type; the bot's own posts come back too.
        if user == self.bot_id || post["type"].as_str().is_some_and(|t| !t.is_empty()) {
            return None;
        }
        if !self.allowed.is_empty() && !self.allowed.iter().any(|c| c == channel) {
            return None;
        }
        let dm = data["channel_type"] == "D";
        let mut text = post["message"].as_str().unwrap_or_default().to_owned();
        let mut mentioned = false;
        if !dm {
            for name in [&self.bot_name, &self.bot_id] {
                let (cleaned, found) = strip_mention(&text, name);
                if found {
                    (text, mentioned) = (cleaned, true);
                }
            }
            if !mentioned && !self.free.iter().any(|c| c == channel) {
                return None;
            }
        }
        let chat_type = match data["channel_type"].as_str() {
            Some("D") => "dm",
            Some("G") => "group",
            _ => "channel",
        };
        let mut message = Inbound::text(id, channel, chat_type, user, &text);
        message.sender_name = data["sender_name"]
            .as_str()
            .map(|n| n.trim_start_matches('@').to_owned());
        // In a channel every admitted post addresses the bot (mention or
        // free-response channel), so the gateway's mention gate passes it.
        message.mentioned = !dm;
        let root = post["root_id"].as_str().filter(|r| !r.is_empty());
        message.thread = match root {
            Some(root) => Some(root.to_owned()),
            None if self.thread_replies && !dm => Some(id.to_owned()),
            None => None,
        };
        message.attachments = self.files(&post);
        Some(message)
    }

    fn upload(&self, channel: &str, file: &message::OutFile) -> Result<String, String> {
        let (content_type, body) = net::multipart(
            &[("channel_id", channel)],
            ("files", &file.name, &file.mime, &file.data),
        );
        let auth = bearer()?;
        let response = net::send(
            "POST",
            &format!("{}/api/v4/files", self.api),
            &[("authorization", &auth), ("content-type", &content_type)],
            Some(body),
        )?;
        net::classify(&response)?;
        response.json()["file_infos"][0]["id"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| "server: the upload returned no file id".to_owned())
    }
}

fn poll_socket(mm: &mut Mattermost) -> Result<Vec<Inbound>, String> {
    let Some(id) = mm.connection.clone() else {
        if clock::now_ms() >= mm.retry_at_ms && mm.connect().is_err() {
            mm.lost();
        }
        return Ok(Vec::new());
    };
    let received = mm.bridge.recv(&id)?;
    let mut inbound = Vec::new();
    for frame in &received.frames {
        let Ok(event) = serde_json::from_str::<Value>(frame) else {
            continue;
        };
        match event["event"].as_str() {
            Some("hello") => mm.backoff_secs = 2,
            Some("posted") => inbound.extend(mm.posted(&event["data"])),
            _ => {}
        }
    }
    if !received.open {
        host::info(&format!(
            "the Mattermost event stream closed ({}); reconnecting in {} s",
            received.error.unwrap_or_default(),
            mm.backoff_secs
        ));
        mm.lost();
    }
    Ok(inbound)
}

fn post(out: &Outgoing) -> Result<String, String> {
    let slot = MM.lock().map_err(|e| e.to_string())?;
    let mm = slot.as_ref().ok_or("not started")?;
    let mut file_ids = Vec::new();
    for file in &out.attachments {
        file_ids.push(mm.upload(&out.chat, file)?);
    }
    let root = out
        .thread
        .clone()
        .or_else(|| out.reply_to.clone().filter(|_| mm.thread_replies));
    let mut body = json!({"channel_id": out.chat, "message": out.text});
    if let Some(root) = root {
        body["root_id"] = json!(root);
    }
    if !file_ids.is_empty() {
        body["file_ids"] = json!(file_ids);
    }
    let answer = net::json(
        "POST",
        &format!("{}/api/v4/posts", mm.api),
        &[("authorization", &bearer()?)],
        &body,
    )?;
    Ok(answer["id"].as_str().unwrap_or_default().to_owned())
}

struct Adapter;

impl Guest for Adapter {
    fn descriptor() -> String {
        json!({
            "platform": "mattermost",
            "max_message_length": MAX_CHARS,
            "length_unit": "chars",
            "splits_long_messages": false,
            "supports_edit": false,
            "supports_delete": false,
            "supports_typing": false,
            "supports_reactions": false,
            "supports_threads": true,
            "interactive": "numbered",
            "media_in": ["image", "audio", "video", "document"],
            "media_out": ["image", "audio", "video", "document"],
            "delivery_receipts": false,
            "read_receipts": false,
            "streaming_tts": false,
            "markup": "markdown",
            "binds_http": false,
        })
        .to_string()
    }

    fn start(options: String) -> Result<(), String> {
        let options: Value = serde_json::from_str(&options).map_err(|e| e.to_string())?;
        let bad = bad_user_ids(&options);
        if !bad.is_empty() {
            return Err(format!(
                "Mattermost allowlists take 26-character user ids, not {}",
                bad.join(", ")
            ));
        }
        let api = options["server_url"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or("options.server_url is required")?
            .trim_end_matches('/')
            .to_owned();
        let thread_replies = match options["reply_mode"].as_str().unwrap_or("off") {
            "off" => false,
            "thread" => true,
            other => {
                return Err(format!(
                    "options.reply_mode: '{other}' is not off or thread"
                ));
            }
        };
        let ws = format!(
            "{}/api/v4/websocket",
            api.replacen("https://", "wss://", 1)
                .replacen("http://", "ws://", 1)
        );
        let me = net::get(
            &format!("{api}/api/v4/users/me"),
            &[("authorization", &bearer()?)],
        )?;
        let mut mm = Mattermost {
            bot_id: me["id"].as_str().unwrap_or_default().to_owned(),
            bot_name: me["username"].as_str().unwrap_or_default().to_owned(),
            allowed: policy::strings(&options, "allowed_channels"),
            free: policy::strings(&options, "free_response_channels"),
            thread_replies,
            bridge: Bridge::from_options(&options)?,
            connection: None,
            backoff_secs: 2,
            retry_at_ms: 0,
            api,
            ws,
        };
        mm.connect()?;
        let mut slot = MM.lock().map_err(|e| e.to_string())?;
        if let Some(mut old) = slot.take() {
            old.lost();
        }
        *slot = Some(mm);
        Ok(())
    }

    fn stop() {
        if let Ok(mut slot) = MM.lock()
            && let Some(mut mm) = slot.take()
        {
            mm.lost();
        }
    }

    fn poll() -> Result<String, String> {
        clock::tick();
        let mut slot = MM.lock().map_err(|e| e.to_string())?;
        let mm = slot.as_mut().ok_or("not started")?;
        poll_socket(mm).map(|inbound| message::batch(&inbound))
    }

    fn send(message: String) -> Result<String, String> {
        post(&Outgoing::parse(&message)?)
    }

    fn chat_info(chat: String) -> Result<String, String> {
        let slot = MM.lock().map_err(|e| e.to_string())?;
        let mm = slot.as_ref().ok_or("not started")?;
        let channel = mm
            .get(&format!("/api/v4/channels/{}", text::percent_encode(&chat)))
            .map_err(|e| e.trim_start_matches("permanent: ").to_owned())?;
        let kind = match channel["type"].as_str() {
            Some("D") => "dm",
            Some("G") => "group",
            _ => "channel",
        };
        let name = channel["display_name"]
            .as_str()
            .filter(|n| !n.is_empty())
            .unwrap_or(&chat);
        Ok(message::chat_info(name, kind))
    }

    fn handle_http(_request: HttpRequest) -> HttpResponse {
        respond(404, "text/plain", "")
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
    fn user_ids_are_26_lower_case_characters() {
        assert!(is_user_id("abcdefghijklmnopqrstuvwxyz"));
        assert!(!is_user_id("alice"));
        assert_eq!(
            bad_user_ids(
                &json!({"access": {"allow_users": ["alice", "a1b2c3d4e5f6g7h8i9j0k1l2m3"]}})
            ),
            ["alice"]
        );
    }

    #[test]
    fn a_mention_is_found_in_any_case_and_removed() {
        assert_eq!(
            strip_mention("hey @PBot what now", "pbot"),
            ("hey what now".to_owned(), true)
        );
        assert!(!strip_mention("hey pbot", "pbot").1);
    }
}

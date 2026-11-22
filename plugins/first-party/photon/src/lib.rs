//! The Photon adapter (spec: platform-adapters, Photon iMessage adapter):
//! iMessage through Photon's managed lines. The sidecar (`photon.mjs`)
//! holds the Spectrum streaming connection and serves inbound messages as
//! an NDJSON stream, which this adapter reads through the sidecar's own
//! socket bridge (a streamed HTTP connection to its `/inbound`), drops
//! repeats by message id, and answers through the sidecar's send
//! endpoints.
//!
//! Inbound files are markers only (`[attachment: name (type)]`), never
//! bytes. Text, files, typing, native polls (choices) and message effects
//! go out: a reply starting with `[effect:<name>]` is sent with that
//! iMessage effect (`slam`, `loud`, `gentle`, `invisible_ink`, ...). A
//! vote on a poll comes back as the chosen button. Read receipts are sent
//! for admitted messages unless `options.read_receipts = false`.

use std::sync::Mutex;

use adapter_kit::bridge::Bridge;
use adapter_kit::message::{self, Inbound, OutFile, Outgoing};
use adapter_kit::{Guest, HttpRequest, HttpResponse, clock, respond, sign};
use serde_json::{Value, json};

const MAX_CHARS: usize = 18000;
const SEEN: usize = 2000;
const MAX_BACKOFF_SECS: u64 = 30;

struct Photon {
    bridge: Bridge,
    sidecar_url: String,
    token: String,
    read_receipts: bool,
    stream: Option<String>,
    retry_at_ms: u64,
    backoff_secs: u64,
    seen: Vec<String>,
    /// chat -> the open poll's (label, data) pairs.
    polls: Vec<(String, Vec<(String, String)>)>,
}

static PH: Mutex<Option<Photon>> = Mutex::new(None);

/// The text of a content object: text as it is, files as markers.
fn text_of(content: &Value) -> String {
    let field = |key: &str| content[key].as_str().unwrap_or_default();
    match field("type") {
        "text" => field("text").to_owned(),
        "richlink" => field("url").to_owned(),
        kind @ ("attachment" | "voice") => {
            let label = if kind == "voice" {
                "voice note"
            } else {
                "attachment"
            };
            let name = content["name"].as_str().unwrap_or("unnamed");
            match content["mimeType"].as_str() {
                Some(mime) => format!("[{label}: {name} ({mime})]"),
                None => format!("[{label}: {name}]"),
            }
        }
        "group" => content["items"]
            .as_array()
            .into_iter()
            .flatten()
            .map(text_of)
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        "reply" => text_of(&content["content"]),
        _ => String::new(),
    }
}

impl Photon {
    fn open(&mut self) {
        let auth = format!("Bearer {}", self.token);
        let url = format!("{}/inbound", self.sidecar_url);
        match self.bridge.open(&url, &[("authorization", &auth)]) {
            Ok(id) => {
                self.stream = Some(id);
                self.backoff_secs = 1;
            }
            Err(_) => self.lost(),
        }
    }

    fn lost(&mut self) {
        if let Some(id) = self.stream.take() {
            self.bridge.close(&id);
        }
        self.retry_at_ms = clock::now_ms() + self.backoff_secs * 1000;
        self.backoff_secs = (self.backoff_secs * 2).min(MAX_BACKOFF_SECS);
    }

    fn post(&self, path: &str, body: &Value) -> Result<Value, String> {
        self.bridge.call(path, body).map_err(|why| {
            if why.starts_with("permanent:")
                || why.starts_with("rate_limited")
                || why.starts_with("server:")
            {
                why
            } else {
                format!("server: {why}")
            }
        })
    }

    fn event(&mut self, line: &str) -> Option<Inbound> {
        let event: Value = serde_json::from_str(line).ok()?;
        let id = event["messageId"].as_str()?.to_owned();
        if self.seen.contains(&id) {
            return None;
        }
        self.seen.push(id.clone());
        if self.seen.len() > SEEN {
            self.seen.remove(0);
        }
        let chat = event["space"]["id"].as_str()?.to_owned();
        let sender = event["sender"]["id"].as_str().unwrap_or(&chat).to_owned();
        let content = &event["content"];
        let group = event["space"]["type"] == "group";
        let mut inbound =
            Inbound::text(&id, &chat, if group { "group" } else { "dm" }, &sender, "");
        if content["type"] == "poll_option" {
            if content["selected"] == false {
                return None;
            }
            let title = content["title"].as_str().unwrap_or_default();
            let data = self
                .polls
                .iter()
                .find(|(c, _)| *c == chat)
                .and_then(|(_, choices)| choices.iter().find(|(label, _)| label == title))
                .map(|(_, data)| data.clone())?;
            inbound.callback = Some(data);
            return Some(inbound);
        }
        inbound.text = text_of(content).trim().to_owned();
        if inbound.text.is_empty() {
            return None;
        }
        // Where polls do not render, a reply that is one of the numbers
        // picks that choice.
        if let Some(data) = self.numbered_choice(&chat, &inbound.text) {
            inbound.callback = Some(data);
        }
        inbound.reply_to = content["targetMessageId"].as_str().map(str::to_owned);
        if self.read_receipts {
            let _ = self.post("/read", &json!({"messageId": id}));
        }
        Some(inbound)
    }

    fn numbered_choice(&self, chat: &str, text: &str) -> Option<String> {
        let n: usize = text.parse().ok()?;
        let (_, choices) = self.polls.iter().find(|(c, _)| c == chat)?;
        choices.get(n.checked_sub(1)?).map(|(_, data)| data.clone())
    }

    fn poll(&mut self) -> Result<Vec<Inbound>, String> {
        let Some(id) = self.stream.clone() else {
            if clock::now_ms() >= self.retry_at_ms {
                self.open();
            }
            return Ok(Vec::new());
        };
        let received = self.bridge.recv(&id)?;
        let inbound = received
            .frames
            .iter()
            .filter_map(|l| self.event(l))
            .collect();
        if !received.open {
            self.lost();
        }
        Ok(inbound)
    }

    fn send_file(&self, chat: &str, file: &OutFile) -> Result<String, String> {
        let kind = if file.as_voice {
            "voice"
        } else {
            file.kind.as_str()
        };
        let answer = self.post(
            "/send-attachment",
            &json!({"spaceId": chat, "name": file.name, "mimeType": file.mime, "kind": kind,
                "data": sign::base64(&file.data)}),
        )?;
        Ok(answer["messageId"].as_str().unwrap_or("sent").to_owned())
    }

    fn send(&mut self, out: &Outgoing) -> Result<String, String> {
        match out.op.as_str() {
            "" => {}
            "typing" => {
                self.post("/typing", &json!({"spaceId": out.chat, "state": "start"}))?;
                return Ok(String::new());
            }
            _ => return Err("unsupported".to_owned()),
        }
        let mut last = None;
        for file in &out.attachments {
            last = Some(self.send_file(&out.chat, file)?);
        }
        let text: String = out.text.chars().take(MAX_CHARS).collect();
        if text.trim().is_empty() {
            return last.ok_or_else(|| "permanent: nothing to send".to_owned());
        }
        let answer = if out.buttons.len() >= 2 {
            let labels: Vec<&str> = out.buttons.iter().map(|b| b.label.as_str()).collect();
            self.polls.retain(|(c, _)| *c != out.chat);
            self.polls.push((
                out.chat.clone(),
                out.buttons
                    .iter()
                    .map(|b| (b.label.clone(), b.data.clone()))
                    .collect(),
            ));
            self.post(
                "/send-poll",
                &json!({"spaceId": out.chat, "title": text, "options": labels}),
            )?
        } else if let Some((effect, rest)) = effect_of(&text) {
            self.post(
                "/send-effect",
                &json!({"spaceId": out.chat, "text": rest, "effect": effect}),
            )?
        } else {
            self.post(
                "/send",
                &json!({"spaceId": out.chat, "text": text, "format": "markdown"}),
            )?
        };
        Ok(answer["messageId"].as_str().unwrap_or("sent").to_owned())
    }
}

/// `[effect:<name>] text` -> (name, text).
fn effect_of(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix("[effect:")?;
    let (name, rest) = rest.split_once(']')?;
    let name = name.trim();
    (!name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
        .then(|| (name, rest.trim_start()))
}

fn with<T>(f: impl FnOnce(&mut Photon) -> Result<T, String>) -> Result<T, String> {
    let mut slot = PH.lock().map_err(|e| e.to_string())?;
    f(slot.as_mut().ok_or("not started")?)
}

struct Adapter;

impl Guest for Adapter {
    fn descriptor() -> String {
        json!({
            "platform": "photon",
            "max_message_length": MAX_CHARS,
            "length_unit": "chars",
            "splits_long_messages": false,
            "supports_edit": false,
            "supports_delete": false,
            "supports_typing": true,
            "supports_reactions": false,
            "supports_threads": false,
            "interactive": "native",
            "media_in": [],
            "media_out": ["image", "voice", "video", "document"],
            "delivery_receipts": false,
            "read_receipts": true,
            "streaming_tts": false,
            "markup": "markdown",
            "binds_http": false,
        })
        .to_string()
    }

    fn start(options: String) -> Result<(), String> {
        let options: Value = serde_json::from_str(&options).map_err(|e| e.to_string())?;
        let bridge = Bridge::from_options(&options)?;
        let mut photon = Photon {
            bridge,
            sidecar_url: options["sidecar"]["url"]
                .as_str()
                .unwrap_or_default()
                .trim_end_matches('/')
                .to_owned(),
            token: options["sidecar"]["token"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            read_receipts: options["read_receipts"].as_bool().unwrap_or(true),
            stream: None,
            retry_at_ms: 0,
            backoff_secs: 1,
            seen: Vec::new(),
            polls: Vec::new(),
        };
        photon.open();
        let mut slot = PH.lock().map_err(|e| e.to_string())?;
        if let Some(mut old) = slot.take() {
            old.lost();
        }
        *slot = Some(photon);
        Ok(())
    }

    fn stop() {
        if let Ok(mut slot) = PH.lock()
            && let Some(mut photon) = slot.take()
        {
            photon.lost();
        }
    }

    fn poll() -> Result<String, String> {
        clock::tick();
        with(Photon::poll).map(|inbound| message::batch(&inbound))
    }

    fn send(message: String) -> Result<String, String> {
        let out = Outgoing::parse(&message)?;
        with(|photon| photon.send(&out))
    }

    fn chat_info(chat: String) -> Result<String, String> {
        // DMs are a phone number or `any;-;+<number>`; anything else is a group.
        let dm = chat.starts_with('+') || chat.starts_with("any;-;");
        Ok(message::chat_info(&chat, if dm { "dm" } else { "group" }))
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
    fn files_are_markers_and_effects_are_parsed() {
        let photo = json!({"type": "attachment", "name": "IMG_1.heic", "mimeType": "image/heic", "size": 9});
        assert_eq!(text_of(&photo), "[attachment: IMG_1.heic (image/heic)]");
        let group = json!({"type": "group", "items": [{"type": "text", "text": "look"}, photo]});
        assert_eq!(
            text_of(&group),
            "look\n[attachment: IMG_1.heic (image/heic)]"
        );
        assert_eq!(effect_of("[effect:slam] Done!"), Some(("slam", "Done!")));
        assert_eq!(effect_of("[effect:] x"), None);
        assert_eq!(effect_of("plain"), None);
    }
}

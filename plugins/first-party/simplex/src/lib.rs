//! The SimpleX adapter (spec: platform-adapters, SimpleX adapter): a
//! local `simplex-chat` daemon's WebSocket API at `options.ws_url`
//! (default `ws://127.0.0.1:5225`), held by the socket bridge.
//!
//! Contacts are opaque numeric contact ids (a direct chat's id is the
//! contact id); allowlists of display names are refused. Contact requests
//! are accepted when `options.auto_accept` (default true): accepting makes
//! a contact, not an authorized one, so the gateway's pairing still
//! decides. Groups are ignored unless listed in `options.allow_groups`
//! (chat id `group:<id>`). Files arrive through the daemon's file
//! transfer: the message waits until the daemon has received the file and
//! carries its path, which the host reads only inside the plugin's
//! `fs_read` grant (the daemon's files folder). Files go out by the path
//! the daemon reads them from.

use std::sync::Mutex;

use adapter_kit::bridge::Bridge;
use adapter_kit::message::{self, InFile, Inbound, Outgoing};
use adapter_kit::{Guest, HttpRequest, HttpResponse, host, policy, respond};
use serde_json::{Value, json};

/// How many bridge reads a command waits for its answer.
const ANSWER_READS: usize = 200;

struct Simplex {
    bridge: Bridge,
    connection: String,
    auto_accept: bool,
    groups: Vec<String>,
    corr: u64,
    /// Events read while waiting for a command's answer.
    backlog: Vec<String>,
    /// Messages waiting for their file: (file id, message).
    awaiting_files: Vec<(u64, Inbound)>,
}

static SIMPLEX: Mutex<Option<Simplex>> = Mutex::new(None);

/// The entries of an allowlist that are not numeric contact ids.
fn display_names(ids: &[String]) -> Vec<String> {
    ids.iter()
        .filter(|id| id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()))
        .cloned()
        .collect()
}

/// The chat and sender of a chat item's `chatInfo`, when it is a direct
/// chat or an allowed group.
fn chat_of(
    info: &Value,
    item: &Value,
    groups: &[String],
) -> Option<(String, &'static str, String)> {
    match info["type"].as_str()? {
        "direct" => {
            let contact = info["contact"]["contactId"].as_u64()?.to_string();
            Some((contact.clone(), "dm", contact))
        }
        "group" => {
            let group = info["groupInfo"]["groupId"].as_u64()?.to_string();
            if !groups.contains(&group) {
                return None;
            }
            let member = &item["chatDir"]["groupMember"];
            let sender = member["memberContactId"].as_u64().map_or_else(
                || format!("member:{}", member["groupMemberId"]),
                |c| c.to_string(),
            );
            Some((format!("group:{group}"), "group", sender))
        }
        _ => None,
    }
}

/// `image`, `video`, `voice` or `document`, from a message content type.
fn file_kind(content: &str) -> &'static str {
    match content {
        "image" => "image",
        "video" => "video",
        "voice" => "voice",
        _ => "document",
    }
}

/// The daemon's address of a chat: `@<contact>` or `#<group>`.
fn target(chat: &str) -> Result<String, String> {
    if let Some(group) = chat.strip_prefix("group:") {
        return Ok(format!("#{group}"));
    }
    if chat.bytes().all(|b| b.is_ascii_digit()) && !chat.is_empty() {
        Ok(format!("@{chat}"))
    } else {
        Err(format!("permanent: '{chat}' is not a SimpleX contact id"))
    }
}

impl Simplex {
    fn command(&mut self, cmd: &str) -> Result<String, String> {
        self.corr += 1;
        let corr = self.corr.to_string();
        let frame = json!({"corrId": corr, "cmd": cmd}).to_string();
        self.bridge.send(&self.connection, &frame)?;
        Ok(corr)
    }

    /// Sends `cmd` and waits for its answer (other events are kept for
    /// the next poll).
    fn ask(&mut self, cmd: &str) -> Result<Value, String> {
        let corr = self.command(cmd)?;
        for _ in 0..ANSWER_READS {
            let received = self.bridge.recv(&self.connection)?;
            let mut answer = None;
            for frame in received.frames {
                let value: Value = serde_json::from_str(&frame).unwrap_or_default();
                if value["corrId"] == corr.as_str() {
                    answer = Some(value["resp"].clone());
                } else {
                    self.backlog.push(frame);
                }
            }
            if let Some(answer) = answer {
                return Ok(answer);
            }
            if !received.open {
                return Err("the simplex-chat connection closed".to_owned());
            }
        }
        Err("ambiguous: simplex-chat did not answer".to_owned())
    }

    /// One daemon event: messages to admit now.
    fn event(&mut self, resp: &Value, inbound: &mut Vec<Inbound>) {
        match resp["type"].as_str() {
            Some("receivedContactRequest") if self.auto_accept => {
                if let Some(id) = resp["contactRequest"]["contactRequestId"].as_u64()
                    && let Err(why) = self.command(&format!("/_accept {id}"))
                {
                    host::warn(&format!(
                        "could not accept a SimpleX contact request: {why}"
                    ));
                }
            }
            Some("newChatItems") => {
                for item in resp["chatItems"].as_array().into_iter().flatten() {
                    inbound.extend(self.chat_item(item));
                }
            }
            Some("rcvFileComplete") => {
                let file = &resp["chatItem"]["chatItem"]["file"];
                let id = file["fileId"].as_u64().unwrap_or_default();
                let path = file["fileSource"]["filePath"].as_str().unwrap_or_default();
                if let Some(at) = self.awaiting_files.iter().position(|(f, _)| *f == id) {
                    let (_, mut message) = self.awaiting_files.remove(at);
                    if let Some(attachment) = message.attachments.first_mut() {
                        attachment.path = Some(path.to_owned());
                    }
                    inbound.push(message);
                }
            }
            _ => {}
        }
    }

    fn chat_item(&mut self, item: &Value) -> Option<Inbound> {
        let chat_item = &item["chatItem"];
        let content = &chat_item["content"];
        if content["type"] != "rcvMsgContent" {
            return None;
        }
        let (chat, kind, sender) = chat_of(&item["chatInfo"], chat_item, &self.groups)?;
        let id = chat_item["meta"]["itemId"].as_u64()?.to_string();
        let msg = &content["msgContent"];
        let mut message = Inbound::text(
            &id,
            &chat,
            kind,
            &sender,
            msg["text"].as_str().unwrap_or_default(),
        );
        message.sender_name = item["chatInfo"]["contact"]["localDisplayName"]
            .as_str()
            .map(str::to_owned);
        // Each group message is a mention only when it names the bot;
        // allowed groups are opted in as a whole.
        message.mentioned = kind == "group";
        let file = &chat_item["file"];
        let Some(file_id) = file["fileId"].as_u64() else {
            return Some(message);
        };
        message.attachments.push(InFile {
            kind: file_kind(msg["type"].as_str().unwrap_or_default()),
            name: file["fileName"].as_str().map(str::to_owned),
            ..InFile::default()
        });
        if let Err(why) = self.command(&format!("/freceive {file_id}")) {
            host::warn(&format!("could not receive a SimpleX file: {why}"));
            message.attachments.clear();
            return Some(message);
        }
        self.awaiting_files.push((file_id, message));
        None
    }
}

/// The message contents of one send: the text, then each file by path.
fn contents(out: &Outgoing) -> Result<Vec<Value>, String> {
    let mut items = Vec::new();
    if !out.text.is_empty() {
        items.push(json!({"msgContent": {"type": "text", "text": out.text}}));
    }
    for file in &out.attachments {
        let path = file
            .path
            .as_deref()
            .ok_or("permanent: SimpleX sends files by path and the host gave none")?;
        let content = match (file.kind.as_str(), file.as_document) {
            ("image", false) => json!({"type": "image", "text": "", "image": ""}),
            ("video", false) => json!({"type": "video", "text": "", "image": "", "duration": 0}),
            ("voice", _) => json!({"type": "voice", "text": "", "duration": 0}),
            _ => json!({"type": "file", "text": ""}),
        };
        items.push(json!({"filePath": path, "msgContent": content}));
    }
    Ok(items)
}

struct Adapter;

impl Guest for Adapter {
    fn descriptor() -> String {
        json!({
            "platform": "simplex",
            "max_message_length": 15000,
            "length_unit": "chars",
            "splits_long_messages": false,
            "supports_edit": false,
            "supports_delete": false,
            "supports_typing": false,
            "supports_reactions": false,
            "supports_threads": false,
            "interactive": "numbered",
            "media_in": ["image", "video", "voice", "document"],
            "media_out": ["image", "video", "voice", "document"],
            "delivery_receipts": false,
            "read_receipts": false,
            "streaming_tts": false,
            "markup": "plain",
            "binds_http": false,
        })
        .to_string()
    }

    fn start(options: String) -> Result<(), String> {
        let options: Value = serde_json::from_str(&options).map_err(|e| e.to_string())?;
        let names = display_names(&policy::strings(&options["access"], "allow_users"));
        if !names.is_empty() {
            return Err(format!(
                "SimpleX allowlists take numeric contact ids, not display names: {}",
                names.join(", ")
            ));
        }
        let url = options["ws_url"].as_str().unwrap_or("ws://127.0.0.1:5225");
        let bridge = Bridge::from_options(&options)?;
        let connection = bridge.open(url, &[])?;
        let simplex = Simplex {
            bridge,
            connection,
            auto_accept: options["auto_accept"].as_bool().unwrap_or(true),
            groups: policy::strings(&options, "allow_groups"),
            corr: 0,
            backlog: Vec::new(),
            awaiting_files: Vec::new(),
        };
        let mut slot = SIMPLEX.lock().map_err(|e| e.to_string())?;
        if let Some(old) = slot.take() {
            old.bridge.close(&old.connection);
        }
        *slot = Some(simplex);
        Ok(())
    }

    fn stop() {
        if let Ok(mut slot) = SIMPLEX.lock()
            && let Some(simplex) = slot.take()
        {
            simplex.bridge.close(&simplex.connection);
        }
    }

    fn poll() -> Result<String, String> {
        let mut slot = SIMPLEX.lock().map_err(|e| e.to_string())?;
        let simplex = slot.as_mut().ok_or("not started")?;
        let received = simplex.bridge.recv(&simplex.connection)?;
        let mut frames = std::mem::take(&mut simplex.backlog);
        frames.extend(received.frames);
        let mut inbound = Vec::new();
        for frame in &frames {
            let value: Value = serde_json::from_str(frame).unwrap_or_default();
            simplex.event(&value["resp"], &mut inbound);
        }
        if !received.open && inbound.is_empty() {
            return Err(format!(
                "the simplex-chat connection closed: {}",
                received.error.unwrap_or_else(|| "by the daemon".to_owned())
            ));
        }
        Ok(message::batch(&inbound))
    }

    fn send(message: String) -> Result<String, String> {
        let out = Outgoing::parse(&message)?;
        let to = target(&out.chat)?;
        let items = contents(&out)?;
        let mut slot = SIMPLEX.lock().map_err(|e| e.to_string())?;
        let simplex = slot.as_mut().ok_or("not started")?;
        let answer = simplex.ask(&format!("/_send {to} json {}", Value::Array(items)))?;
        if answer["type"] != "newChatItems" {
            return Err(format!(
                "permanent: simplex-chat refused the message: {}",
                answer["chatError"]
            ));
        }
        Ok(answer["chatItems"][0]["chatItem"]["meta"]["itemId"]
            .as_u64()
            .map(|id| id.to_string())
            .unwrap_or_default())
    }

    fn chat_info(chat: String) -> Result<String, String> {
        let kind = if chat.starts_with("group:") {
            "group"
        } else {
            "dm"
        };
        target(&chat)?;
        Ok(message::chat_info(&chat, kind))
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
    fn allowlists_take_numeric_contact_ids() {
        assert_eq!(display_names(&["12".into(), "alice".into()]), ["alice"]);
        assert!(display_names(&["3".into()]).is_empty());
    }

    #[test]
    fn chats_address_contacts_and_groups() {
        assert_eq!(target("7").unwrap(), "@7");
        assert_eq!(target("group:3").unwrap(), "#3");
        assert!(target("alice").unwrap_err().starts_with("permanent:"));
    }
}

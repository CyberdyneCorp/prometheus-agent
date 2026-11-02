//! Bridge events as the gateway's inbound events (spec: platform-adapters,
//! Inbound event normalization). `WhatsApp` names one person by a phone JID
//! (`<number>@s.whatsapp.net`) or a LID (`<id>@lid`); the bridge reports
//! the other identifier when it knows it, and the adapter remembers each
//! LID's phone JID (in `<profile>/platforms/whatsapp/aliases.json`) so a
//! person is always the same sender and their direct chat the same chat,
//! whichever identifier a message arrives under.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use jiff::Timestamp;
use pmt_gateway::messaging::{Attachment, ChatType, Inbound, MediaKind};
use serde_json::Value;

const PHONE: &str = "@s.whatsapp.net";
const LID: &str = "@lid";
const GROUP: &str = "@g.us";

fn text(value: &Value) -> Option<String> {
    value.as_str().filter(|s| !s.is_empty()).map(str::to_owned)
}

/// LID to phone JID pairs, kept on disk.
#[derive(Debug)]
pub(crate) struct Aliases {
    file: PathBuf,
    known: Mutex<BTreeMap<String, String>>,
}

impl Aliases {
    /// The pairs stored in `file` (none when it is missing or unreadable).
    pub(crate) fn load(file: PathBuf) -> Self {
        let known = std::fs::read(&file)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Self {
            file,
            known: Mutex::new(known),
        }
    }

    /// Learns that `a` and `b` name one person (one a LID, one a phone).
    fn learn(&self, a: &str, b: &str) {
        let (lid, phone) = match (a.ends_with(LID), b.ends_with(LID)) {
            (true, false) if b.ends_with(PHONE) => (a, b),
            (false, true) if a.ends_with(PHONE) => (b, a),
            _ => return,
        };
        let mut known = self.known.lock().expect("aliases poisoned");
        if known.get(lid).map(String::as_str) == Some(phone) {
            return;
        }
        known.insert(lid.to_owned(), phone.to_owned());
        let saved = serde_json::to_vec_pretty(&*known)
            .map_err(std::io::Error::other)
            .and_then(|bytes| {
                if let Some(dir) = self.file.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                std::fs::write(&self.file, bytes)
            });
        if let Err(err) = saved {
            tracing::warn!(
                platform = crate::PLATFORM,
                "cannot store a WhatsApp alias: {err}"
            );
        }
    }

    /// The canonical id of `id`: a known LID becomes its phone JID.
    pub(crate) fn canonical(&self, id: &str) -> String {
        self.known
            .lock()
            .expect("aliases poisoned")
            .get(id)
            .cloned()
            .unwrap_or_else(|| id.to_owned())
    }
}

fn kind(media_type: &str) -> MediaKind {
    match media_type {
        "image" | "sticker" => MediaKind::Image,
        "ptt" | "voice" => MediaKind::Voice,
        "audio" => MediaKind::Audio,
        "video" => MediaKind::Video,
        _ => MediaKind::Document,
    }
}

/// A bridge event as an inbound event; `None` for the account's own
/// messages and events without a sender.
pub(crate) fn event(event: &Value, aliases: &Aliases) -> Option<Inbound> {
    if event["fromMe"].as_bool() == Some(true) {
        return None;
    }
    let raw_sender = text(&event["senderId"])?;
    if let Some(alt) = text(&event["senderAltId"]) {
        aliases.learn(&raw_sender, &alt);
    }
    let sender = aliases.canonical(&raw_sender);
    let raw_chat = text(&event["chatId"])?;
    let group = raw_chat.ends_with(GROUP) || event["isGroup"].as_bool() == Some(true);
    let chat = if group {
        raw_chat
    } else {
        aliases.canonical(&raw_chat)
    };
    let media_type = event["mediaType"].as_str().unwrap_or_default();
    let attachments = event["mediaUrls"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|url| Some(Attachment::new(kind(media_type), url.as_str()?)))
        .collect();
    Some(Inbound {
        message_id: text(&event["messageId"])?,
        sender_id: sender,
        sender_name: text(&event["senderName"]),
        chat_id: chat,
        chat_type: if group { ChatType::Group } else { ChatType::Dm },
        scope_id: None,
        thread_id: None,
        timestamp: event["timestamp"]
            .as_i64()
            .and_then(|s| Timestamp::from_second(s).ok())
            .unwrap_or_else(Timestamp::now),
        text: event["body"].as_str().unwrap_or_default().to_owned(),
        attachments,
        reply_to: text(&event["quotedMessageId"]),
        is_bot: false,
        callback: None,
        profile: None,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_person_is_one_sender_and_one_chat_under_either_identifier() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("aliases.json");
        let aliases = Aliases::load(file.clone());
        let by_phone = json!({"messageId": "A1", "chatId": "15551234567@s.whatsapp.net",
            "senderId": "15551234567@s.whatsapp.net", "senderAltId": "98765@lid", "body": "hi"});
        let by_lid = json!({"messageId": "A2", "chatId": "98765@lid",
            "senderId": "98765@lid", "body": "again"});
        let first = event(&by_phone, &aliases).unwrap();
        let second = event(&by_lid, &aliases).unwrap();
        assert_eq!(first.sender_id, second.sender_id);
        assert_eq!(first.chat_id, second.chat_id);
        assert_eq!(second.sender_id, "15551234567@s.whatsapp.net");
        // The pair survives a restart.
        let reloaded = Aliases::load(file);
        assert_eq!(
            reloaded.canonical("98765@lid"),
            "15551234567@s.whatsapp.net"
        );
    }

    #[test]
    fn groups_keep_their_id_and_own_messages_are_not_inbound() {
        let aliases = Aliases::load(PathBuf::from("/nonexistent/aliases.json"));
        let group = json!({"messageId": "G1", "chatId": "123@g.us", "isGroup": true,
            "senderId": "1@s.whatsapp.net", "body": "x", "mediaType": "ptt",
            "mediaUrls": ["/cache/a.ogg"]});
        let inbound = event(&group, &aliases).unwrap();
        assert_eq!(inbound.chat_type, ChatType::Group);
        assert_eq!(inbound.chat_id, "123@g.us");
        assert_eq!(inbound.attachments[0].kind, MediaKind::Voice);
        let own = json!({"messageId": "M", "chatId": "1@s.whatsapp.net", "senderId": "1@s.whatsapp.net", "fromMe": true});
        assert_eq!(event(&own, &aliases), None);
    }
}

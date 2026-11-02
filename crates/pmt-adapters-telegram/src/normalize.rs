//! Telegram updates as the gateway's inbound events (spec:
//! platform-adapters, Inbound event normalization): a message's numeric
//! ids become strings, private chats are DMs, a forum topic is the thread,
//! the largest photo size stands for the photo, and a pressed inline
//! button is a callback carrying its data.

use jiff::Timestamp;
use pmt_gateway::messaging::{Attachment, ChatType, Inbound, MediaKind};
use serde_json::Value;

/// One update's event, and the callback query to answer (a button press).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Update {
    pub(crate) inbound: Inbound,
    pub(crate) callback_query: Option<String>,
}

/// A JSON id (number or string) as text.
pub(crate) fn id(value: &Value) -> Option<String> {
    match value {
        Value::Number(n) => Some(n.to_string()),
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

/// The chat kind of a Telegram chat type.
pub(crate) fn chat_type(kind: &str) -> ChatType {
    match kind {
        "private" => ChatType::Dm,
        "channel" => ChatType::Channel,
        _ => ChatType::Group,
    }
}

fn sender_name(from: &Value) -> Option<String> {
    let first = from["first_name"].as_str().unwrap_or_default();
    let last = from["last_name"].as_str().unwrap_or_default();
    let full = format!("{first} {last}").trim().to_owned();
    if full.is_empty() {
        from["username"].as_str().map(str::to_owned)
    } else {
        Some(full)
    }
}

fn attachments(message: &Value) -> Vec<Attachment> {
    let mut found = Vec::new();
    if let Some(largest) = message["photo"].as_array().and_then(|sizes| sizes.last()) {
        found.extend(file(largest, MediaKind::Image));
    }
    for (key, kind) in [
        ("voice", MediaKind::Voice),
        ("audio", MediaKind::Audio),
        ("video", MediaKind::Video),
        ("video_note", MediaKind::Video),
        ("document", MediaKind::Document),
    ] {
        if message[key].is_object() {
            found.extend(file(&message[key], kind));
        }
    }
    found
}

fn file(value: &Value, kind: MediaKind) -> Option<Attachment> {
    let mut attachment = Attachment::new(kind, value["file_id"].as_str()?);
    attachment.name = value["file_name"].as_str().map(str::to_owned);
    attachment.duration_secs = value["duration"].as_u64();
    Some(attachment)
}

fn message_event(message: &Value) -> Option<Inbound> {
    let chat = &message["chat"];
    let from = &message["from"];
    let text = message["text"]
        .as_str()
        .or_else(|| message["caption"].as_str())
        .unwrap_or_default();
    let mut inbound = Inbound::text(
        chat_type(chat["type"].as_str().unwrap_or_default()),
        &id(&chat["id"])?,
        &id(&from["id"]).or_else(|| id(&chat["id"]))?,
        text,
    );
    inbound.message_id = id(&message["message_id"])?;
    inbound.sender_name = sender_name(from);
    inbound.is_bot = from["is_bot"].as_bool().unwrap_or(false);
    if message["is_topic_message"].as_bool() == Some(true) {
        inbound.thread_id = id(&message["message_thread_id"]);
    }
    inbound.reply_to = id(&message["reply_to_message"]["message_id"]);
    inbound.timestamp = message["date"]
        .as_i64()
        .and_then(|secs| Timestamp::from_second(secs).ok())
        .unwrap_or_else(Timestamp::now);
    inbound.attachments = attachments(message);
    Some(inbound)
}

fn callback_event(query: &Value) -> Option<Update> {
    let message = &query["message"];
    let chat = &message["chat"];
    let from = &query["from"];
    let mut inbound = Inbound::text(
        chat_type(chat["type"].as_str().unwrap_or_default()),
        &id(&chat["id"])?,
        &id(&from["id"])?,
        "",
    );
    let query_id = id(&query["id"])?;
    inbound.message_id = format!("cb:{query_id}");
    inbound.sender_name = sender_name(from);
    inbound.callback = query["data"].as_str().map(str::to_owned);
    inbound.reply_to = id(&message["message_id"]);
    if message["is_topic_message"].as_bool() == Some(true) {
        inbound.thread_id = id(&message["message_thread_id"]);
    }
    Some(Update {
        inbound,
        callback_query: Some(query_id),
    })
}

/// The event of one update; `None` for updates the gateway ignores.
pub(crate) fn update(update: &Value) -> Option<Update> {
    if update["callback_query"].is_object() {
        return callback_event(&update["callback_query"]);
    }
    let inbound = message_event(&update["message"])?;
    Some(Update {
        inbound,
        callback_query: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_topic_photo_message_is_normalized() {
        let raw = json!({"update_id": 9, "message": {
            "message_id": 77, "date": 1_700_000_000,
            "chat": {"id": -1001, "type": "supergroup"},
            "from": {"id": 42, "first_name": "Ada", "is_bot": false},
            "is_topic_message": true, "message_thread_id": 5,
            "caption": "look",
            "photo": [{"file_id": "small"}, {"file_id": "large"}],
            "reply_to_message": {"message_id": 70}
        }});
        let event = update(&raw).unwrap().inbound;
        assert_eq!(event.message_id, "77");
        assert_eq!(event.chat_id, "-1001");
        assert_eq!(event.chat_type, ChatType::Group);
        assert_eq!(event.sender_id, "42");
        assert_eq!(event.sender_name.as_deref(), Some("Ada"));
        assert_eq!(event.thread_id.as_deref(), Some("5"));
        assert_eq!(event.text, "look");
        assert_eq!(event.reply_to.as_deref(), Some("70"));
        assert_eq!(event.attachments[0].location, "large");
        assert_eq!(event.attachments[0].kind, MediaKind::Image);
    }

    #[test]
    fn a_button_press_is_a_callback() {
        let raw = json!({"update_id": 10, "callback_query": {
            "id": "q1", "data": "appr:x:once:y",
            "from": {"id": 42, "first_name": "Ada"},
            "message": {"message_id": 80, "chat": {"id": 42, "type": "private"}}
        }});
        let event = update(&raw).unwrap();
        assert_eq!(event.callback_query.as_deref(), Some("q1"));
        assert_eq!(event.inbound.callback.as_deref(), Some("appr:x:once:y"));
        assert_eq!(event.inbound.chat_type, ChatType::Dm);
    }
}

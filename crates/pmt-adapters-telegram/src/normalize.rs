//! Telegram updates as the gateway's inbound events (spec:
//! platform-adapters, Inbound event normalization): a message's numeric
//! ids become strings, private chats are DMs, a forum topic is the thread,
//! the largest photo size stands for the photo, and a pressed inline
//! button is a callback carrying its data.

use jiff::Timestamp;
use pmt_gateway::messaging::{Addressing, Attachment, ChatType, Inbound, MediaKind};
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

/// The bot as Telegram names it (`getMe`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Me {
    pub(crate) id: String,
    pub(crate) username: String,
}

impl Me {
    pub(crate) fn of(get_me: &Value) -> Self {
        Self {
            id: id(&get_me["id"]).unwrap_or_default(),
            username: get_me["username"].as_str().unwrap_or_default().to_owned(),
        }
    }
}

/// How `message` addresses the bot: an `@username` or a text mention of
/// it, or a reply to one of its messages (not the topic's opening
/// message, which every topic message replies to).
pub(crate) fn addressing(message: &Value, me: &Me) -> Addressing {
    let text = message["text"]
        .as_str()
        .or_else(|| message["caption"].as_str())
        .unwrap_or_default()
        .to_lowercase();
    let handle = format!("@{}", me.username.to_lowercase());
    let named = !me.username.is_empty()
        && text.match_indices(&handle).any(|(at, _)| {
            text[at + handle.len()..]
                .chars()
                .next()
                .is_none_or(|c| !c.is_alphanumeric() && c != '_')
        });
    let entities = message["entities"]
        .as_array()
        .or_else(|| message["caption_entities"].as_array());
    let text_mention = entities.into_iter().flatten().any(|e| {
        e["type"] == "text_mention" && id(&e["user"]["id"]).as_deref() == Some(me.id.as_str())
    });
    let reply = &message["reply_to_message"];
    let replies_to_bot = !reply["forum_topic_created"].is_object()
        && !me.id.is_empty()
        && id(&reply["from"]["id"]).as_deref() == Some(me.id.as_str());
    Addressing {
        mentioned: named || text_mention,
        replies_to_bot,
        ..Addressing::default()
    }
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
pub(crate) fn update(update: &Value, me: &Me) -> Option<Update> {
    if update["callback_query"].is_object() {
        return callback_event(&update["callback_query"]);
    }
    let mut inbound = message_event(&update["message"])?;
    inbound.addressing = addressing(&update["message"], me);
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
    fn mentions_and_replies_to_the_bot_are_recognised() {
        let me = Me {
            id: "99".into(),
            username: "PromBot".into(),
        };
        let at = json!({"text": "hey @prombot, look", "from": {"id": 1}});
        assert!(addressing(&at, &me).mentioned);
        let other = json!({"text": "hey @prombotter"});
        assert!(!addressing(&other, &me).mentioned);
        let reply = json!({"text": "yes", "reply_to_message": {"from": {"id": 99}}});
        assert!(addressing(&reply, &me).replies_to_bot);
        let topic = json!({"text": "yes", "reply_to_message":
            {"from": {"id": 99}, "forum_topic_created": {"name": "t"}}});
        assert!(!addressing(&topic, &me).replies_to_bot);
    }

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
        let event = update(&raw, &Me::default()).unwrap().inbound;
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
        let event = update(&raw, &Me::default()).unwrap();
        assert_eq!(event.callback_query.as_deref(), Some("q1"));
        assert_eq!(event.inbound.callback.as_deref(), Some("appr:x:once:y"));
        assert_eq!(event.inbound.chat_type, ChatType::Dm);
    }
}

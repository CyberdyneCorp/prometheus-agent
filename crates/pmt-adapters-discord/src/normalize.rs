//! Discord Gateway events as the gateway's inbound events (spec:
//! platform-adapters, Inbound event normalization): a server message is a
//! group message with the server as scope, a DM channel message is a DM,
//! attachments are typed by their content type (a voice message by its
//! flag), and a pressed button is a callback carrying its custom id.

use std::time::Duration;

use jiff::Timestamp;
use pmt_gateway::messaging::{Attachment, ChatType, Inbound, MediaKind};
use serde_json::Value;

/// The `IS_VOICE_MESSAGE` message flag.
const VOICE_MESSAGE: u64 = 1 << 13;

fn text(value: &Value) -> Option<String> {
    value.as_str().filter(|s| !s.is_empty()).map(str::to_owned)
}

fn kind(content_type: &str, voice: bool) -> MediaKind {
    match content_type.split('/').next().unwrap_or_default() {
        "image" => MediaKind::Image,
        "audio" if voice => MediaKind::Voice,
        "audio" => MediaKind::Audio,
        "video" => MediaKind::Video,
        _ => MediaKind::Document,
    }
}

fn attachments(message: &Value) -> Vec<Attachment> {
    let voice = message["flags"].as_u64().unwrap_or(0) & VOICE_MESSAGE != 0;
    message["attachments"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| {
            let content_type = a["content_type"].as_str().unwrap_or_default();
            let mut attachment = Attachment::new(kind(content_type, voice), a["url"].as_str()?);
            attachment.name = text(&a["filename"]);
            attachment.duration_secs = a["duration_secs"].as_f64().map(|s| {
                Duration::from_secs_f64(s.max(0.0)).as_secs() + u64::from(s.fract() > 0.0)
            });
            Some(attachment)
        })
        .collect()
}

/// A `MESSAGE_CREATE` as an event; `None` for the bot's own messages and,
/// with `require_mention`, server messages that do not mention it.
pub(crate) fn message(message: &Value, bot_id: &str, require_mention: bool) -> Option<Inbound> {
    let author = &message["author"];
    let sender = text(&author["id"])?;
    if sender == bot_id {
        return None;
    }
    let guild = text(&message["guild_id"]);
    let mentioned = message["mentions"]
        .as_array()
        .is_some_and(|m| m.iter().any(|u| u["id"].as_str() == Some(bot_id)));
    if guild.is_some() && require_mention && !mentioned {
        return None;
    }
    let chat_type = if guild.is_some() {
        ChatType::Group
    } else {
        ChatType::Dm
    };
    let content = message["content"].as_str().unwrap_or_default();
    let mut inbound = Inbound::text(chat_type, &text(&message["channel_id"])?, &sender, content);
    inbound.message_id = text(&message["id"])?;
    inbound.scope_id = guild;
    inbound.sender_name = text(&author["global_name"]).or_else(|| text(&author["username"]));
    inbound.is_bot = author["bot"].as_bool().unwrap_or(false);
    inbound.reply_to = text(&message["message_reference"]["message_id"]);
    inbound.timestamp = message["timestamp"]
        .as_str()
        .and_then(|t| t.parse::<Timestamp>().ok())
        .unwrap_or_else(Timestamp::now);
    inbound.attachments = attachments(message);
    Some(inbound)
}

/// A component interaction (a pressed button) as an event.
pub(crate) fn interaction(interaction: &Value) -> Option<Inbound> {
    if interaction["type"].as_u64() != Some(3) {
        return None;
    }
    let user = if interaction["member"].is_object() {
        &interaction["member"]["user"]
    } else {
        &interaction["user"]
    };
    let chat_type = if interaction["guild_id"].is_string() {
        ChatType::Group
    } else {
        ChatType::Dm
    };
    let mut inbound = Inbound::text(
        chat_type,
        &text(&interaction["channel_id"])?,
        &text(&user["id"])?,
        "",
    );
    inbound.message_id = format!("ix:{}", text(&interaction["id"])?);
    inbound.scope_id = text(&interaction["guild_id"]);
    inbound.sender_name = text(&user["global_name"]).or_else(|| text(&user["username"]));
    inbound.callback = text(&interaction["data"]["custom_id"]);
    inbound.reply_to = text(&interaction["message"]["id"]);
    Some(inbound)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_server_message_with_a_voice_note_is_normalized() {
        let raw = json!({
            "id": "m1", "channel_id": "c1", "guild_id": "g1", "flags": 8192,
            "author": {"id": "u1", "username": "ada", "global_name": "Ada"},
            "content": "", "timestamp": "2026-11-02T10:00:00+00:00",
            "mentions": [{"id": "bot"}],
            "attachments": [{"url": "https://cdn/x.ogg", "filename": "voice-message.ogg",
                             "content_type": "audio/ogg", "duration_secs": 3.2}]
        });
        let event = message(&raw, "bot", true).unwrap();
        assert_eq!(event.chat_type, ChatType::Group);
        assert_eq!(event.scope_id.as_deref(), Some("g1"));
        assert_eq!(event.sender_name.as_deref(), Some("Ada"));
        assert_eq!(event.attachments[0].kind, MediaKind::Voice);
        assert_eq!(event.attachments[0].duration_secs, Some(4));
        let unmentioned = json!({"id": "m2", "channel_id": "c1", "guild_id": "g1",
                                 "author": {"id": "u1"}, "content": "hi", "mentions": []});
        assert!(message(&unmentioned, "bot", true).is_none());
        assert!(message(&unmentioned, "bot", false).is_some());
        let own = json!({"id": "m3", "channel_id": "c1", "author": {"id": "bot"}, "content": "x"});
        assert!(message(&own, "bot", false).is_none());
    }

    #[test]
    fn a_pressed_button_is_a_callback() {
        let raw = json!({"id": "i1", "token": "t", "type": 3, "channel_id": "dm1",
                         "user": {"id": "u1"}, "data": {"custom_id": "appr:a:once:b"},
                         "message": {"id": "m9"}});
        let event = interaction(&raw).unwrap();
        assert_eq!(event.callback.as_deref(), Some("appr:a:once:b"));
        assert_eq!(event.chat_type, ChatType::Dm);
        assert_eq!(event.reply_to.as_deref(), Some("m9"));
    }
}

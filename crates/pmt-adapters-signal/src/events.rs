//! Inbound Signal traffic: the daemon's server-sent events
//! (`GET /api/v1/events`), each a `receive` envelope, normalized into
//! inbound events (spec: platform-adapters, Inbound event normalization).
//! A message's id is `<author>:<timestamp>`, which is how Signal names a
//! message for quotes, reactions and edits. Run the daemon with
//! `--receive-mode on-connection` so Signal holds messages while the
//! adapter is disconnected.

use std::sync::Arc;

use jiff::Timestamp;
use pmt_gateway::messaging::{
    AdapterError, AdapterLink, AdapterState, Attachment, ChatType, Inbound, MediaKind,
};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

/// The chat id prefix of a group.
pub(crate) const GROUP: &str = "group.";

fn text(value: &Value) -> Option<String> {
    value.as_str().filter(|s| !s.is_empty()).map(str::to_owned)
}

/// A phone number as logs show it (`+155****4567`).
#[must_use]
pub(crate) fn redact(number: &str) -> String {
    let chars: Vec<char> = number.chars().collect();
    if chars.len() <= 8 {
        return "*".repeat(chars.len());
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}****{tail}")
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
    message["attachments"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| {
            let content_type = a["contentType"].as_str().unwrap_or_default();
            let voice = a["voiceNote"].as_bool().unwrap_or(false);
            let mut attachment = Attachment::new(kind(content_type, voice), a["id"].as_str()?);
            attachment.name = text(&a["filename"]);
            Some(attachment)
        })
        .collect()
}

/// A `receive` notification as an inbound event; `None` for receipts,
/// typing notices and sync messages.
pub(crate) fn envelope(data: &Value) -> Option<Inbound> {
    let envelope = if data["envelope"].is_object() {
        &data["envelope"]
    } else {
        &data["params"]["envelope"]
    };
    let message = &envelope["dataMessage"];
    if !message.is_object() {
        return None;
    }
    let sender = text(&envelope["sourceNumber"]).or_else(|| text(&envelope["sourceUuid"]))?;
    let sent = message["timestamp"]
        .as_i64()
        .or_else(|| envelope["timestamp"].as_i64())?;
    let group = text(&message["groupInfo"]["groupId"]);
    let quote = &message["quote"];
    let reply_to = match (
        text(&quote["authorNumber"]).or_else(|| text(&quote["author"])),
        quote["id"].as_i64(),
    ) {
        (Some(author), Some(id)) => Some(format!("{author}:{id}")),
        _ => None,
    };
    Some(Inbound {
        message_id: format!("{sender}:{sent}"),
        sender_id: sender.clone(),
        sender_name: text(&envelope["sourceName"]),
        chat_id: group
            .as_ref()
            .map_or_else(|| sender.clone(), |g| format!("{GROUP}{g}")),
        chat_type: if group.is_some() {
            ChatType::Group
        } else {
            ChatType::Dm
        },
        scope_id: None,
        thread_id: None,
        timestamp: Timestamp::from_millisecond(sent).unwrap_or_else(|_| Timestamp::now()),
        text: message["message"].as_str().unwrap_or_default().to_owned(),
        attachments: attachments(message),
        reply_to,
        is_bot: false,
        callback: None,
        profile: None,
    })
}

/// Reads the event stream until stopped or it ends.
pub(crate) async fn stream(
    mut response: reqwest::Response,
    link: Arc<dyn AdapterLink>,
    url: String,
    stop: CancellationToken,
) {
    let mut buffer = String::new();
    loop {
        let chunk = tokio::select! {
            () = stop.cancelled() => return,
            chunk = response.chunk() => chunk,
        };
        let Ok(Some(chunk)) = chunk else { break };
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(end) = buffer.find("\n\n") {
            let event: String = buffer.drain(..end + 2).collect();
            let data: String = event
                .lines()
                .filter_map(|l| l.strip_prefix("data:"))
                .map(str::trim_start)
                .collect::<Vec<_>>()
                .join("\n");
            let parsed: Value = serde_json::from_str(&data).unwrap_or(Value::Null);
            if let Some(inbound) = envelope(&parsed) {
                link.submit(inbound).await;
            }
        }
    }
    tracing::warn!(
        platform = crate::PLATFORM,
        "the signal-cli event stream ended"
    );
    link.report(
        AdapterState::Retrying,
        Some(AdapterError {
            retryable: true,
            code: "signal_daemon_unreachable".to_owned(),
            message: format!("the signal-cli daemon at {url} closed the event stream"),
        }),
    );
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_direct_message_is_keyed_by_author_and_timestamp() {
        let data = json!({"envelope": {"sourceNumber": "+15550001111", "sourceName": "Kit",
            "timestamp": 1_700_000_000_123_i64,
            "dataMessage": {"timestamp": 1_700_000_000_123_i64, "message": "hi",
                            "quote": {"id": 1_700_000_000_000_i64, "authorNumber": "+15559998888"}}}});
        let inbound = envelope(&data).unwrap();
        assert_eq!(inbound.message_id, "+15550001111:1700000000123");
        assert_eq!(inbound.chat_id, "+15550001111");
        assert_eq!(inbound.chat_type, ChatType::Dm);
        assert_eq!(
            inbound.reply_to.as_deref(),
            Some("+15559998888:1700000000000")
        );
    }

    #[test]
    fn group_messages_live_in_the_group_and_receipts_are_ignored() {
        let data = json!({"params": {"envelope": {"sourceUuid": "uuid-1", "timestamp": 5,
            "dataMessage": {"timestamp": 5, "message": "x", "groupInfo": {"groupId": "abc=="},
                            "attachments": [{"id": "att1", "contentType": "audio/aac", "voiceNote": true}]}}}});
        let inbound = envelope(&data).unwrap();
        assert_eq!(inbound.chat_id, "group.abc==");
        assert_eq!(inbound.chat_type, ChatType::Group);
        assert_eq!(inbound.attachments[0].kind, MediaKind::Voice);
        let receipt =
            json!({"envelope": {"sourceNumber": "+1555", "receiptMessage": {"isRead": true}}});
        assert_eq!(envelope(&receipt), None);
    }

    #[test]
    fn numbers_are_redacted_for_logs() {
        assert_eq!(redact("+15551234567"), "+155****4567");
        assert_eq!(redact("+1555"), "*****");
    }
}

//! Inbound Signal traffic: the daemon's server-sent events
//! (`GET /api/v1/events`), each a `receive` envelope, normalized into
//! inbound events (spec: platform-adapters, Inbound event normalization).
//! A message's id is `<author>:<timestamp>`, which is how Signal names a
//! message for quotes, reactions and edits. Run the daemon with
//! `--receive-mode on-connection` so Signal holds messages while the
//! adapter is disconnected.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use jiff::Timestamp;
use pmt_gateway::messaging::{
    AdapterError, AdapterLink, AdapterState, Attachment, ChatType, Inbound, MediaKind,
};
use serde_json::{Value, json};
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

/// What reading the event stream needs to know.
#[derive(Debug, Clone, Default)]
pub(crate) struct Reader {
    /// The account's own number (Note to Self).
    pub(crate) account: String,
    /// Groups admitted: `None` none, `["*"]` all.
    pub(crate) allow_groups: Option<Vec<String>>,
    /// Timestamps of the adapter's own sends (their sync copies are not
    /// inbound).
    pub(crate) sent: Arc<std::sync::Mutex<VecDeque<i64>>>,
    /// Texts of own sends to the account in flight (their echo may arrive
    /// before the send's answer names its timestamp).
    pub(crate) to_self: Arc<std::sync::Mutex<VecDeque<String>>>,
}

/// Own sends remembered for echo suppression.
const SENT_KEPT: usize = 1000;

impl Reader {
    /// Remembers an own send's timestamp.
    pub(crate) fn sent(&self, timestamp: i64) {
        let mut sent = self.sent.lock().expect("sent poisoned");
        if sent.len() >= SENT_KEPT {
            sent.pop_front();
        }
        sent.push_back(timestamp);
    }

    /// Notes a send to the account itself before it goes out.
    pub(crate) fn sending_to_self(&self, text: &str) {
        let mut pending = self.to_self.lock().expect("sent poisoned");
        if pending.len() >= 50 {
            pending.pop_front();
        }
        pending.push_back(text.to_owned());
    }

    /// Whether a sync copy is the echo of an own send.
    fn own_echo(&self, timestamp: i64, text: &str) -> bool {
        if self
            .sent
            .lock()
            .expect("sent poisoned")
            .contains(&timestamp)
        {
            return true;
        }
        let mut pending = self.to_self.lock().expect("sent poisoned");
        match pending.iter().position(|t| t == text) {
            Some(i) => {
                pending.remove(i);
                true
            }
            None => false,
        }
    }

    fn group_admitted(&self, group: &str) -> bool {
        self.allow_groups.as_ref().is_some_and(|allowed| {
            allowed
                .iter()
                .any(|g| g == "*" || g == group || g.strip_prefix(GROUP) == Some(group))
        })
    }

    /// A `receive` notification as an inbound event; `None` for receipts,
    /// typing notices, sync copies of other devices' sends, the adapter's
    /// own sends, and groups not admitted. A message the account sends to
    /// itself (Note to Self) is inbound.
    pub(crate) fn envelope(&self, data: &Value) -> Option<Inbound> {
        let envelope = if data["envelope"].is_object() {
            &data["envelope"]
        } else {
            &data["params"]["envelope"]
        };
        let sent = &envelope["syncMessage"]["sentMessage"];
        let note_to_self = sent.is_object()
            && text(&sent["destinationNumber"]).or_else(|| text(&sent["destination"]))
                == Some(self.account.clone());
        if note_to_self {
            let timestamp = sent["timestamp"].as_i64()?;
            if self.own_echo(timestamp, sent["message"].as_str().unwrap_or_default()) {
                return None;
            }
            let mut synthetic = envelope.clone();
            synthetic["dataMessage"] = sent.clone();
            synthetic["sourceNumber"] = json!(self.account);
            return message(&synthetic);
        }
        let mut inbound = message(envelope)?;
        if inbound.chat_type == ChatType::Group
            && !self.group_admitted(inbound.chat_id.trim_start_matches(GROUP))
        {
            return None;
        }
        inbound.addressing = addressing(&envelope["dataMessage"], &self.account);
        Some(inbound)
    }
}

/// How a data message addresses `account`: one of its mentions names the
/// account, or it quotes one of the account's messages.
fn addressing(message: &Value, account: &str) -> pmt_gateway::messaging::Addressing {
    let names_account = |v: &Value| {
        text(&v["number"]).as_deref() == Some(account)
            || text(&v["authorNumber"]).as_deref() == Some(account)
            || text(&v["author"]).as_deref() == Some(account)
    };
    pmt_gateway::messaging::Addressing {
        mentioned: message["mentions"]
            .as_array()
            .is_some_and(|m| m.iter().any(names_account)),
        replies_to_bot: names_account(&message["quote"]),
        ..pmt_gateway::messaging::Addressing::default()
    }
}

/// A data message of `envelope` as an inbound event.
fn message(envelope: &Value) -> Option<Inbound> {
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
        interaction: None,
        profile: None,
        addressing: pmt_gateway::messaging::Addressing::default(),
    })
}

/// How long the stream may stay silent before the daemon is pinged.
pub(crate) const QUIET: Duration = Duration::from_mins(2);

/// Reads the event stream until stopped, it ends, or the daemon stops
/// answering its health check after a quiet spell.
pub(crate) async fn stream(
    mut response: reqwest::Response,
    link: Arc<dyn AdapterLink>,
    reader: &Reader,
    rpc: &crate::rpc::Rpc,
    stop: &CancellationToken,
) {
    let mut buffer = String::new();
    loop {
        let chunk = tokio::select! {
            () = stop.cancelled() => return,
            chunk = tokio::time::timeout(QUIET, response.chunk()) => chunk,
        };
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(_) if rpc.check().await.is_ok() => continue,
            Err(_) => break,
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
            if let Some(inbound) = reader.envelope(&parsed) {
                link.submit(inbound).await;
            }
        }
    }
}

/// Backoff of the stream's reconnects.
const BACKOFF: (Duration, Duration) = (Duration::from_secs(2), Duration::from_mins(1));

/// Streams events until stopped, reconnecting a lost stream itself with a
/// backoff from 2 to 60 seconds (the adapter is `degraded` meanwhile).
pub(crate) async fn follow(
    first: reqwest::Response,
    link: Arc<dyn AdapterLink>,
    reader: Reader,
    rpc: crate::rpc::Rpc,
    stop: CancellationToken,
) {
    let mut response = Some(first);
    let mut wait = BACKOFF.0;
    loop {
        if let Some(open) = response.take() {
            stream(open, Arc::clone(&link), &reader, &rpc, &stop).await;
            if stop.is_cancelled() {
                return;
            }
            tracing::warn!(
                platform = crate::PLATFORM,
                "the signal-cli event stream ended"
            );
            link.report(
                AdapterState::Degraded,
                Some(AdapterError {
                    retryable: true,
                    code: "signal_daemon_unreachable".to_owned(),
                    message: format!(
                        "the signal-cli daemon at {} closed the event stream",
                        rpc.url
                    ),
                }),
            );
        }
        tokio::select! {
            () = stop.cancelled() => return,
            () = tokio::time::sleep(wait) => {}
        }
        match rpc.events().await {
            Ok(open) => {
                link.report(AdapterState::Connected, None);
                response = Some(open);
                wait = BACKOFF.0;
            }
            Err(_) => wait = (wait * 2).min(BACKOFF.1),
        }
    }
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
        let inbound = Reader::default().envelope(&data).unwrap();
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
        assert_eq!(
            Reader::default().envelope(&data),
            None,
            "groups are off by default"
        );
        let all = Reader {
            allow_groups: Some(vec!["*".into()]),
            ..Reader::default()
        };
        let inbound = all.envelope(&data).unwrap();
        assert_eq!(inbound.chat_id, "group.abc==");
        assert_eq!(inbound.chat_type, ChatType::Group);
        assert_eq!(inbound.attachments[0].kind, MediaKind::Voice);
        let receipt =
            json!({"envelope": {"sourceNumber": "+1555", "receiptMessage": {"isRead": true}}});
        assert_eq!(all.envelope(&receipt), None);
        let listed = Reader {
            allow_groups: Some(vec!["group.abc==".into()]),
            ..Reader::default()
        };
        assert!(listed.envelope(&data).is_some());
        let other = Reader {
            allow_groups: Some(vec!["xyz==".into()]),
            ..Reader::default()
        };
        assert!(other.envelope(&data).is_none());
    }

    #[test]
    fn note_to_self_is_inbound_and_the_agents_own_reply_is_not() {
        let reader = Reader {
            account: "+15550000001".into(),
            ..Reader::default()
        };
        let note = |ts: i64, text: &str| {
            json!({"envelope": {"sourceNumber": "+15550000001", "timestamp": ts,
                "syncMessage": {"sentMessage": {"destinationNumber": "+15550000001",
                                                "timestamp": ts, "message": text}}}})
        };
        let inbound = reader.envelope(&note(10, "remember the milk")).unwrap();
        assert_eq!(inbound.text, "remember the milk");
        assert_eq!(inbound.chat_id, "+15550000001");
        reader.sent(11);
        assert_eq!(
            reader.envelope(&note(11, "Noted.")),
            None,
            "the reply is not re-admitted"
        );
        let elsewhere = json!({"envelope": {"sourceNumber": "+15550000001", "timestamp": 12,
            "syncMessage": {"sentMessage": {"destinationNumber": "+15559990000",
                                            "timestamp": 12, "message": "hi friend"}}}});
        assert_eq!(reader.envelope(&elsewhere), None);
    }

    #[test]
    fn numbers_are_redacted_for_logs() {
        assert_eq!(redact("+15551234567"), "+155****4567");
        assert_eq!(redact("+1555"), "*****");
    }
}

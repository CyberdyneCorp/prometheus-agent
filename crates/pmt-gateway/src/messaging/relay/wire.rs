//! The relay connector's wire format, contract version 1: JSON text frames
//! over one WebSocket the gateway dials.
//!
//! - connector → gateway, first: `{"type":"descriptor","contract_version":1,
//!   "platforms":[...],"max_message_length":N,"supports_edit":bool}`
//!   (unknown keys ignored; a missing or non-positive length is 4096);
//! - connector → gateway: `{"type":"inbound","platform":..,"profile"?:..,
//!   "message_id","sender_id","sender_name"?,"chat_id","chat_type",
//!   "thread_id"?,"text","reply_to"?,"is_bot"?,"callback"?}`, answered by
//!   `{"type":"admission","message_id":..,"admission":..}`;
//! - gateway → connector: `{"type":"send"|"edit"|"egress",
//!   "id":..,...}`, answered by `{"type":"result","id":..,"ok":true,
//!   "message_id"?,"allow"?}` or `{"type":"result","id":..,"ok":false,
//!   "error":{"kind":"rate_limited|network|server|permanent|unsupported",
//!   "message":..,"retry_after_ms"?}}`.

use std::time::Duration;

use serde_json::{Value, json};

use crate::messaging::adapter::{Inbound, Outgoing, SendError};
use crate::messaging::key::ChatType;

/// The only contract version this build speaks.
pub(crate) const CONTRACT_VERSION: u64 = 1;
/// The length used when the descriptor declares none.
pub(crate) const DEFAULT_MAX_LENGTH: usize = 4096;

/// What the connector fronts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RelayDescriptor {
    /// Platforms it carries.
    pub(crate) platforms: Vec<String>,
    /// Longest message.
    pub(crate) max_message_length: usize,
    /// Messages can be edited.
    pub(crate) supports_edit: bool,
}

/// Parses the descriptor frame; `Err` names what is wrong.
pub(crate) fn descriptor(frame: &Value) -> Result<RelayDescriptor, String> {
    if frame["type"] != "descriptor" {
        return Err("the first frame is not a descriptor".to_owned());
    }
    let version = frame["contract_version"].as_u64();
    if version != Some(CONTRACT_VERSION) {
        return Err(format!(
            "unsupported relay contract_version {}",
            frame["contract_version"]
        ));
    }
    let platforms: Vec<String> = frame["platforms"]
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(Value::as_str)
                .map(str::to_lowercase)
                .collect()
        })
        .unwrap_or_default();
    let max_message_length = match frame["max_message_length"].as_i64() {
        Some(n) if n > 0 => usize::try_from(n).unwrap_or(DEFAULT_MAX_LENGTH),
        other => {
            tracing::warn!(
                declared = ?other,
                "relay descriptor has no positive max_message_length; using {DEFAULT_MAX_LENGTH}"
            );
            DEFAULT_MAX_LENGTH
        }
    };
    Ok(RelayDescriptor {
        platforms,
        max_message_length,
        supports_edit: frame["supports_edit"].as_bool().unwrap_or(false),
    })
}

fn text(frame: &Value, key: &str) -> Option<String> {
    frame[key].as_str().map(str::to_owned)
}

/// An inbound frame as the platform it came from and the event.
pub(crate) fn inbound(frame: &Value) -> Option<(String, Inbound)> {
    let platform = frame["platform"].as_str()?.to_lowercase();
    let inbound = Inbound {
        message_id: text(frame, "message_id").unwrap_or_default(),
        sender_id: text(frame, "sender_id")?,
        sender_name: text(frame, "sender_name"),
        chat_id: text(frame, "chat_id")?,
        chat_type: ChatType::parse(frame["chat_type"].as_str().unwrap_or("dm"))?,
        scope_id: text(frame, "scope_id"),
        thread_id: text(frame, "thread_id"),
        timestamp: jiff::Timestamp::now(),
        text: text(frame, "text").unwrap_or_default(),
        attachments: Vec::new(),
        reply_to: text(frame, "reply_to"),
        is_bot: frame["is_bot"].as_bool().unwrap_or(false),
        callback: text(frame, "callback"),
        interaction: None,
        profile: text(frame, "profile"),
        addressing: crate::messaging::adapter::Addressing::default(),
    };
    Some((platform, inbound))
}

/// A `send` or `edit` request.
pub(crate) fn outgoing(
    id: &str,
    platform: &str,
    message: &Outgoing,
    edit_of: Option<&str>,
) -> Value {
    json!({
        "type": if edit_of.is_some() { "edit" } else { "send" },
        "id": id,
        "platform": platform,
        "message_id": edit_of,
        "chat_id": message.chat_id,
        "thread_id": message.thread_id,
        "reply_to": message.reply_to,
        "text": message.text,
        "buttons": message.buttons,
        "attachments": message.attachments,
    })
}

/// An `egress` request: may the gateway send to this chat?
pub(crate) fn egress(id: &str, platform: &str, chat_id: &str) -> Value {
    json!({"type": "egress", "id": id, "platform": platform, "chat_id": chat_id})
}

/// A result frame as a send outcome.
pub(crate) fn send_result(result: &Value) -> Result<Option<String>, SendError> {
    if result["ok"].as_bool() == Some(true) {
        return Ok(text(result, "message_id"));
    }
    let error = &result["error"];
    let message = error["message"]
        .as_str()
        .unwrap_or("the relay refused the message")
        .to_owned();
    Err(match error["kind"].as_str() {
        Some("rate_limited") => SendError::RateLimited {
            retry_after: error["retry_after_ms"].as_u64().map(Duration::from_millis),
        },
        Some("network") => SendError::Network(message),
        Some("server") => SendError::Server(message),
        Some("unsupported") => SendError::Unsupported,
        _ => SendError::Permanent(message),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_descriptor_ignores_unknown_keys_and_defaults_the_length() {
        let parsed = descriptor(&json!({
            "type": "descriptor",
            "contract_version": 1,
            "platforms": ["Slack", "teams"],
            "max_message_length": 0,
            "colour": "blue"
        }))
        .unwrap();
        assert_eq!(parsed.platforms, vec!["slack", "teams"]);
        assert_eq!(parsed.max_message_length, DEFAULT_MAX_LENGTH);
        assert!(!parsed.supports_edit);
        assert!(descriptor(&json!({"type": "descriptor", "contract_version": 2})).is_err());
        assert!(descriptor(&json!({"type": "inbound"})).is_err());
    }

    #[test]
    fn result_errors_map_to_send_errors() {
        assert_eq!(
            send_result(&json!({"ok": true, "message_id": "m1"})),
            Ok(Some("m1".into()))
        );
        assert_eq!(
            send_result(
                &json!({"ok": false, "error": {"kind": "rate_limited", "retry_after_ms": 1500}})
            ),
            Err(SendError::RateLimited {
                retry_after: Some(Duration::from_millis(1500))
            })
        );
        assert!(matches!(
            send_result(&json!({"ok": false, "error": {"kind": "weird", "message": "no"}})),
            Err(SendError::Permanent(m)) if m == "no"
        ));
    }
}

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
//!   "id":..,"profile"?:..,...}`, answered by `{"type":"result","id":..,"ok":true,
//!   "message_id"?,"allow"?}` or `{"type":"result","id":..,"ok":false,
//!   "error":{"kind":"rate_limited|network|server|permanent|unsupported",
//!   "message":..,"retry_after_ms"?}}`.
//!
//! Optional additions a forwarding adapter host (`daemon run
//! --adapters-only`) uses, ignored by older peers: the descriptor's
//! `descriptors` (each adapter's own descriptor, by platform), an
//! inbound frame's `addressing` and `attachments` (`[{kind, location,
//! name?, duration_secs?}]`, opened later with a `media` request
//! `{"type":"media","id","platform","location"}` answered by a result with
//! base64 `data`), and outgoing attachments carrying their bytes as
//! base64 `data` beside `name`.

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::{Value, json};

use crate::messaging::adapter::{
    Addressing, Attachment, Descriptor, Inbound, MediaKind, Outgoing, SendError,
};
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
    /// The adapters' own descriptors a forwarding host declares, by
    /// platform.
    pub(crate) descriptors: BTreeMap<String, Descriptor>,
}

impl RelayDescriptor {
    /// How the gateway sees relayed `platform`: the host adapter's
    /// presentation (length, unit, edits, threads, buttons, media, markup)
    /// when it declared one, else the connector-wide limits; never more
    /// operations than the relay carries.
    pub(crate) fn descriptor_of(&self, platform: &str) -> Descriptor {
        let mut out = Descriptor::new(platform, self.max_message_length);
        out.supports_edit = self.supports_edit;
        if let Some(own) = self.descriptors.get(platform) {
            out.max_message_length = own.max_message_length;
            out.length_unit = own.length_unit;
            out.splits_long_messages = own.splits_long_messages;
            out.supports_edit = own.supports_edit;
            out.supports_threads = own.supports_threads;
            out.interactive = own.interactive;
            out.media_in.clone_from(&own.media_in);
            out.media_out.clone_from(&own.media_out);
            out.markup = own.markup;
        }
        out
    }
}

/// The descriptor frame a forwarding host sends for its adapters.
pub(crate) fn descriptor_frame(descriptors: &BTreeMap<String, Descriptor>) -> Value {
    let max = descriptors
        .values()
        .map(|d| d.max_message_length)
        .min()
        .unwrap_or(DEFAULT_MAX_LENGTH);
    json!({
        "type": "descriptor",
        "contract_version": CONTRACT_VERSION,
        "platforms": descriptors.keys().collect::<Vec<_>>(),
        "max_message_length": max,
        "supports_edit": !descriptors.is_empty() && descriptors.values().all(|d| d.supports_edit),
        "descriptors": descriptors,
    })
}

fn positive(value: &Value) -> Option<usize> {
    value
        .as_i64()
        .filter(|n| *n > 0)
        .and_then(|n| usize::try_from(n).ok())
}

/// The declared adapter descriptors; an invalid one is left out.
fn descriptors(frame: &Value) -> BTreeMap<String, Descriptor> {
    let Some(map) = frame["descriptors"].as_object() else {
        return BTreeMap::new();
    };
    map.iter()
        .filter_map(|(platform, value)| {
            Descriptor::from_value(value)
                .map_err(|e| {
                    tracing::warn!(platform, "relay descriptor of {platform} ignored: {e}");
                })
                .ok()
                .map(|d| (platform.to_lowercase(), d))
        })
        .collect()
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
    let max_message_length = positive(&frame["max_message_length"]).unwrap_or_else(|| {
        tracing::warn!(
            declared = %frame["max_message_length"],
            "relay descriptor has no positive max_message_length; using {DEFAULT_MAX_LENGTH}"
        );
        DEFAULT_MAX_LENGTH
    });
    Ok(RelayDescriptor {
        platforms,
        max_message_length,
        supports_edit: frame["supports_edit"].as_bool().unwrap_or(false),
        descriptors: descriptors(frame),
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
        addressing: addressing(&frame["addressing"]),
    };
    let inbound = Inbound {
        attachments: attachments(&frame["attachments"]),
        ..inbound
    };
    Some((platform, inbound))
}

fn addressing(value: &Value) -> Addressing {
    let flag = |key: &str| value[key].as_bool().unwrap_or(false);
    Addressing {
        mentioned: flag("mentioned"),
        replies_to_bot: flag("replies_to_bot"),
        in_bot_thread: flag("in_bot_thread"),
        not_addressed: flag("not_addressed"),
    }
}

fn attachments(value: &Value) -> Vec<Attachment> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| {
            let kind = serde_json::from_value::<MediaKind>(a["kind"].clone()).ok()?;
            Some(Attachment {
                kind,
                location: a["location"].as_str()?.to_owned(),
                name: text(a, "name"),
                duration_secs: a["duration_secs"].as_u64(),
            })
        })
        .collect()
}

/// The inbound frame a forwarding host sends for an admitted event of
/// `profile`'s `platform`.
pub(crate) fn inbound_frame(profile: &str, platform: &str, inbound: &Inbound) -> Value {
    let a = inbound.addressing;
    let attachments: Vec<Value> = inbound
        .attachments
        .iter()
        .map(|at| {
            json!({"kind": at.kind, "location": at.location, "name": at.name,
                   "duration_secs": at.duration_secs})
        })
        .collect();
    json!({
        "type": "inbound",
        "platform": platform,
        "profile": profile,
        "message_id": inbound.message_id,
        "sender_id": inbound.sender_id,
        "sender_name": inbound.sender_name,
        "chat_id": inbound.chat_id,
        "chat_type": inbound.chat_type.as_str(),
        "scope_id": inbound.scope_id,
        "thread_id": inbound.thread_id,
        "text": inbound.text,
        "reply_to": inbound.reply_to,
        "is_bot": inbound.is_bot,
        "callback": inbound.callback,
        "addressing": {"mentioned": a.mentioned, "replies_to_bot": a.replies_to_bot,
                       "in_bot_thread": a.in_bot_thread, "not_addressed": a.not_addressed},
        "attachments": attachments,
    })
}

/// A `send` or `edit` request.
pub(crate) fn outgoing(
    id: &str,
    (profile, platform): (&str, &str),
    message: &Outgoing,
    edit_of: Option<&str>,
) -> Value {
    let mut frame = json!({
        "type": if edit_of.is_some() { "edit" } else { "send" },
        "id": id,
        "profile": profile,
        "platform": platform,
        "message_id": edit_of,
        "chat_id": message.chat_id,
        "thread_id": message.thread_id,
        "reply_to": message.reply_to,
        "text": message.text,
        "buttons": message.buttons,
        "attachments": message.attachments,
    });
    if message.plain {
        frame["plain"] = Value::Bool(true);
    }
    if let Some(notify) = message.notify {
        frame["notify"] = Value::Bool(notify);
    }
    frame
}

/// A `media` request: the bytes of an inbound attachment.
pub(crate) fn media(id: &str, (profile, platform): (&str, &str), location: &str) -> Value {
    json!({"type": "media", "id": id, "profile": profile, "platform": platform,
           "location": location})
}

/// A send or edit request as the message to hand a local adapter (its
/// attachments still as sent: [`crate::messaging::forward`] writes the
/// carried bytes to files first).
pub(crate) fn parse_outgoing(frame: &Value) -> Option<Outgoing> {
    Some(Outgoing {
        chat_id: text(frame, "chat_id")?,
        thread_id: text(frame, "thread_id"),
        reply_to: text(frame, "reply_to"),
        text: text(frame, "text").unwrap_or_default(),
        buttons: serde_json::from_value(frame["buttons"].clone()).unwrap_or_default(),
        select: None,
        attachments: Vec::new(),
        plain: frame["plain"].as_bool().unwrap_or(false),
        notify: frame["notify"].as_bool(),
    })
}

/// A successful result frame for request `id`, with `extra` fields.
pub(crate) fn ok_result(id: &Value, extra: Value) -> Value {
    let mut frame = json!({"type": "result", "id": id, "ok": true});
    if let (Some(frame), Value::Object(extra)) = (frame.as_object_mut(), extra) {
        frame.extend(extra);
    }
    frame
}

/// A failed result frame for request `id`.
pub(crate) fn error_result(id: &Value, error: &SendError) -> Value {
    let (kind, message, retry) = match error {
        SendError::RateLimited { retry_after } => (
            "rate_limited",
            "rate limited".to_owned(),
            retry_after.map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
        ),
        SendError::Network(m) | SendError::Ambiguous(m) => ("network", m.clone(), None),
        SendError::Server(m) => ("server", m.clone(), None),
        SendError::Unsupported => ("unsupported", "unsupported".to_owned(), None),
        other => ("permanent", other.describe(), None),
    };
    json!({"type": "result", "id": id, "ok": false,
           "error": {"kind": kind, "message": message, "retry_after_ms": retry}})
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
    fn a_forwarded_event_keeps_its_addressing_attachments_and_profile() {
        let mut event = Inbound::text(ChatType::Group, "!room", "@ana", "look");
        event.message_id = "e1".into();
        event.addressing.mentioned = true;
        event.attachments = vec![Attachment {
            name: Some("photo.jpg".into()),
            ..Attachment::new(MediaKind::Image, "fwd:1:ab")
        }];
        let (platform, parsed) = inbound(&inbound_frame("work", "matrix", &event)).unwrap();
        assert_eq!(platform, "matrix");
        assert_eq!(parsed.profile.as_deref(), Some("work"));
        assert!(parsed.addressing.mentioned);
        assert_eq!(parsed.attachments, event.attachments);
        assert_eq!(
            (parsed.chat_type, parsed.text.as_str()),
            (ChatType::Group, "look")
        );
    }

    #[test]
    fn a_hosts_descriptors_shape_each_relayed_platform() {
        let mut matrix = Descriptor::new("matrix", 32_000);
        matrix.supports_edit = true;
        matrix.media_in.insert(MediaKind::Image);
        matrix.supports_typing = true;
        let declared = BTreeMap::from([
            ("matrix".to_owned(), matrix),
            ("irc".to_owned(), Descriptor::new("irc", 450)),
        ]);
        let parsed = descriptor(&descriptor_frame(&declared)).unwrap();
        assert_eq!(parsed.max_message_length, 450);
        let seen = parsed.descriptor_of("matrix");
        assert_eq!(seen.max_message_length, 32_000);
        assert!(seen.supports_edit && seen.media_in.contains(&MediaKind::Image));
        // Typing is not carried by the relay.
        assert!(!seen.supports_typing);
        // A connector without descriptors keeps its connector-wide limits.
        let plain = descriptor(&json!({"type": "descriptor", "contract_version": 1,
            "platforms": ["slack"], "max_message_length": 39000, "supports_edit": true}))
        .unwrap();
        let slack = plain.descriptor_of("slack");
        assert_eq!(
            (slack.max_message_length, slack.supports_edit),
            (39_000, true)
        );
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

//! Slack events as the gateway's inbound events (spec: platform-adapters,
//! Inbound event normalization): an `im` message is a DM, every other
//! conversation a group with the workspace as scope, a message's `ts` is
//! its id (Slack redelivers with the same `ts`), a reply in a thread
//! carries the thread's root `ts`, files are typed by their MIME type,
//! and a pressed Block Kit button is a callback carrying its value.

use jiff::Timestamp;
use pmt_gateway::messaging::{Attachment, ChatType, Inbound, MediaKind, Select};
use serde_json::Value;

/// Message subtypes that carry a human's (or bot's) new message; edits,
/// deletions, joins and the like are not inbound messages.
const NEW_MESSAGE: &[&str] = &[
    "file_share",
    "thread_broadcast",
    "bot_message",
    "me_message",
];

fn text(value: &Value) -> Option<String> {
    value.as_str().filter(|s| !s.is_empty()).map(str::to_owned)
}

/// A Slack `ts` (`1700000000.000100`) as a timestamp.
pub(crate) fn timestamp(ts: &str) -> Timestamp {
    let seconds: f64 = ts.parse().unwrap_or(0.0);
    #[allow(clippy::cast_possible_truncation)] // whole seconds of a ts
    let whole = seconds.trunc() as i64;
    Timestamp::from_second(whole).unwrap_or_else(|_| Timestamp::now())
}

fn kind(mime: &str, subtype: &str) -> MediaKind {
    match mime.split('/').next().unwrap_or_default() {
        "image" => MediaKind::Image,
        "audio" if subtype == "slack_audio" => MediaKind::Voice,
        "audio" => MediaKind::Audio,
        "video" => MediaKind::Video,
        _ => MediaKind::Document,
    }
}

fn attachments(event: &Value) -> Vec<Attachment> {
    event["files"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|f| {
            let url = f["url_private_download"]
                .as_str()
                .or_else(|| f["url_private"].as_str())?;
            let mime = f["mimetype"].as_str().unwrap_or_default();
            let subtype = f["subtype"].as_str().unwrap_or_default();
            let mut attachment = Attachment::new(kind(mime, subtype), url);
            attachment.name = text(&f["name"]);
            Some(attachment)
        })
        .collect()
}

fn chat_type(channel_type: &str) -> ChatType {
    match channel_type {
        "im" => ChatType::Dm,
        _ => ChatType::Group,
    }
}

/// A `message` event as an inbound event; `None` for the bot's own
/// messages and for edits, deletions and membership notices.
pub(crate) fn message(event: &Value, team: Option<&str>, bot_user: &str) -> Option<Inbound> {
    if event["type"].as_str() != Some("message") {
        return None;
    }
    let subtype = event["subtype"].as_str().unwrap_or_default();
    if !subtype.is_empty() && !NEW_MESSAGE.contains(&subtype) {
        return None;
    }
    let sender = text(&event["user"]).or_else(|| text(&event["bot_id"]))?;
    if sender == bot_user {
        return None;
    }
    let ts = text(&event["ts"])?;
    let thread = text(&event["thread_ts"]).filter(|t| *t != ts);
    Some(Inbound {
        message_id: ts.clone(),
        sender_id: sender,
        sender_name: text(&event["user_profile"]["display_name"])
            .or_else(|| text(&event["username"])),
        chat_id: text(&event["channel"])?,
        chat_type: chat_type(event["channel_type"].as_str().unwrap_or_default()),
        scope_id: text(&event["team"]).or_else(|| team.map(str::to_owned)),
        thread_id: thread,
        timestamp: timestamp(&ts),
        text: event["text"].as_str().unwrap_or_default().to_owned(),
        attachments: attachments(event),
        reply_to: None,
        is_bot: event["bot_id"].is_string() || subtype == "bot_message",
        callback: None,
        profile: None,
        addressing: pmt_gateway::messaging::Addressing {
            mentioned: !bot_user.is_empty()
                && event["text"]
                    .as_str()
                    .is_some_and(|t| t.contains(&format!("<@{bot_user}>"))),
            in_bot_thread: !bot_user.is_empty()
                && event["parent_user_id"].as_str() == Some(bot_user),
            ..pmt_gateway::messaging::Addressing::default()
        },
    })
}

/// The action id of a multi-select's checkboxes.
pub(crate) const SELECT_ACTION: &str = "pmt_select";
/// The action id of a multi-select's Submit button.
pub(crate) const SUBMIT_ACTION: &str = "pmt_submit";

/// The checked boxes of a multi-select, from the payload's state.
fn checked(payload: &Value) -> Vec<usize> {
    let mut numbers: Vec<usize> = payload["state"]["values"]
        .as_object()
        .into_iter()
        .flat_map(|blocks| blocks.values())
        .filter_map(|actions| actions.get(SELECT_ACTION))
        .flat_map(|select| {
            select["selected_options"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .filter_map(|o| o["value"].as_str().and_then(|v| v.parse().ok()))
        .collect();
    numbers.sort_unstable();
    numbers
}

/// A `block_actions` payload (a pressed button, or a multi-select's
/// Submit) as a callback event; ticking a box alone is not one.
pub(crate) fn action(payload: &Value) -> Option<Inbound> {
    if payload["type"].as_str() != Some("block_actions") {
        return None;
    }
    let pressed = payload["actions"].as_array()?.first()?;
    if pressed["action_id"] == SELECT_ACTION {
        return None;
    }
    let mut data = text(&pressed["value"])?;
    if pressed["action_id"] == SUBMIT_ACTION {
        data = Select::submission(&data, &checked(payload));
    }
    let chat =
        text(&payload["channel"]["id"]).or_else(|| text(&payload["container"]["channel_id"]))?;
    let message = text(&payload["message"]["ts"])
        .or_else(|| text(&payload["container"]["message_ts"]))
        .unwrap_or_default();
    let mut inbound = Inbound::text(
        if chat.starts_with('D') {
            ChatType::Dm
        } else {
            ChatType::Group
        },
        &chat,
        &text(&payload["user"]["id"])?,
        "",
    );
    // Each press is its own event: the trigger id is unique per press.
    inbound.message_id = format!(
        "{message}:{}",
        text(&payload["trigger_id"])
            .or_else(|| text(&pressed["action_ts"]))
            .unwrap_or_default()
    );
    inbound.sender_name = text(&payload["user"]["username"]);
    inbound.scope_id = text(&payload["team"]["id"]);
    inbound.thread_id = text(&payload["message"]["thread_ts"]).filter(|t| *t != message);
    inbound.reply_to = Some(message).filter(|m| !m.is_empty());
    inbound.callback = Some(data);
    Some(inbound)
}

#[cfg(test)]
mod select_tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_submitted_multi_select_carries_the_checked_numbers() {
        let state = json!({"values": {"b1": {SELECT_ACTION: {"type": "checkboxes",
            "selected_options": [{"value": "3"}, {"value": "1"}]}}}});
        let submit = json!({"type": "block_actions", "trigger_id": "t1", "user": {"id": "U1"},
            "channel": {"id": "D1"}, "message": {"ts": "1.1"}, "state": state,
            "actions": [{"action_id": SUBMIT_ACTION, "value": "clar:x:0.m:mac"}]});
        assert_eq!(
            action(&submit).unwrap().callback.as_deref(),
            Some("clar:x:0.m:mac|1,3")
        );
        let tick = json!({"type": "block_actions", "user": {"id": "U1"}, "channel": {"id": "D1"},
            "actions": [{"action_id": SELECT_ACTION, "selected_options": []}]});
        assert!(action(&tick).is_none());
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_dm_message_is_a_dm_keyed_by_its_ts() {
        let event = json!({"type": "message", "channel": "D1", "channel_type": "im",
            "user": "U1", "text": "hi", "ts": "1700000000.000100", "team": "T1"});
        let inbound = message(&event, None, "UBOT").unwrap();
        assert_eq!(inbound.message_id, "1700000000.000100");
        assert_eq!(inbound.chat_type, ChatType::Dm);
        assert_eq!(inbound.scope_id.as_deref(), Some("T1"));
        assert_eq!(inbound.timestamp.as_second(), 1_700_000_000);
        assert!(!inbound.is_bot);
    }

    #[test]
    fn edits_own_messages_and_notices_are_not_inbound() {
        let own = json!({"type": "message", "channel": "C1", "user": "UBOT", "ts": "1.0"});
        let edit =
            json!({"type": "message", "subtype": "message_changed", "channel": "C1", "ts": "1.0"});
        let join = json!({"type": "message", "subtype": "channel_join", "user": "U1", "channel": "C1", "ts": "1.0"});
        for event in [own, edit, join] {
            assert_eq!(message(&event, None, "UBOT"), None, "{event}");
        }
    }

    #[test]
    fn a_thread_reply_carries_its_root() {
        let event = json!({"type": "message", "channel": "C1", "channel_type": "channel",
            "user": "U1", "text": "x", "ts": "2.0", "thread_ts": "1.0",
            "files": [{"mimetype": "image/png", "name": "a.png", "url_private_download": "https://f/a"}]});
        let inbound = message(&event, Some("T9"), "UBOT").unwrap();
        assert_eq!(inbound.chat_type, ChatType::Group);
        assert_eq!(inbound.thread_id.as_deref(), Some("1.0"));
        assert_eq!(inbound.scope_id.as_deref(), Some("T9"));
        assert_eq!(inbound.attachments[0].kind, MediaKind::Image);
    }
}

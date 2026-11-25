//! The `/sync` loop and event normalization (spec: platform-adapters,
//! Adapter contract; Inbound event normalization). The `since` cursor
//! survives reconnects, so the homeserver replays everything the adapter
//! missed; the first connect only takes the current cursor (history is not
//! answered). A two-member room is a DM; a reply in a thread carries the
//! thread root; which rooms are read and invites accepted, and stale
//! events, are [`crate::rooms`]'s.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use jiff::Timestamp;
use pmt_gateway::messaging::{
    AdapterError, AdapterLink, AdapterState, Attachment, ChatType, Inbound, MediaKind, SendError,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::api::{self, Api, segment};

/// Seconds one `/sync` long poll waits.
const POLL_SECS: u64 = 30;

/// What survives reconnects.
#[derive(Debug, Default)]
pub(crate) struct Cursor {
    pub(crate) since: Option<String>,
    /// Joined member counts by room (two members: a DM).
    pub(crate) members: HashMap<String, usize>,
}

/// One connection's context.
pub(crate) struct Run {
    pub(crate) api: Api,
    pub(crate) link: Arc<dyn AdapterLink>,
    pub(crate) user_id: String,
    pub(crate) cursor: Arc<Mutex<Cursor>>,
    pub(crate) allow_users: Vec<String>,
    pub(crate) rooms: Arc<crate::rooms::Rooms>,
    /// End-to-end encryption, when running.
    pub(crate) crypto: Option<Arc<crate::crypto::Crypto>>,
    pub(crate) encrypted: crate::e2ee::Encrypted,
    /// Rooms already warned about an undecryptable event.
    pub(crate) undecryptable: Arc<Mutex<std::collections::HashSet<String>>>,
    pub(crate) stop: CancellationToken,
}

fn text(value: &Value) -> Option<String> {
    value.as_str().filter(|s| !s.is_empty()).map(str::to_owned)
}

fn media(content: &Value) -> Option<Attachment> {
    let kind = match content["msgtype"].as_str()? {
        "m.image" => MediaKind::Image,
        "m.audio" if content["org.matrix.msc3245.voice"].is_object() => MediaKind::Voice,
        "m.audio" => MediaKind::Audio,
        "m.video" => MediaKind::Video,
        "m.file" => MediaKind::Document,
        _ => return None,
    };
    // An encrypted attachment carries its URL and key under `file`.
    let location = if content["file"].is_object() {
        crate::crypto::attachment::location(&content["file"])?
    } else {
        content["url"].as_str()?.to_owned()
    };
    let mut attachment = Attachment::new(kind, &location);
    attachment.name = text(&content["filename"]).or_else(|| text(&content["body"]));
    attachment.duration_secs = content["info"]["duration"]
        .as_u64()
        .map(|ms| ms.div_ceil(1000));
    Some(attachment)
}

/// How a message's content addresses user `me`: listed in its intentional
/// mentions (`m.mentions`), or named in its body or a link to it in the
/// formatted body (clients without intentional mentions).
fn addressing(content: &Value, me: &str) -> pmt_gateway::messaging::Addressing {
    let listed = content["m.mentions"]["user_ids"]
        .as_array()
        .is_some_and(|ids| ids.iter().any(|id| id.as_str() == Some(me)));
    let named = !me.is_empty()
        && (content["body"].as_str().is_some_and(|b| b.contains(me))
            || content["formatted_body"]
                .as_str()
                .is_some_and(|b| b.contains(&format!("matrix.to/#/{me}"))));
    pmt_gateway::messaging::Addressing {
        mentioned: listed || named,
        ..pmt_gateway::messaging::Addressing::default()
    }
}

/// A timeline event as an inbound event; `None` for the adapter's own
/// events, edits and anything but messages.
pub(crate) fn message(event: &Value, room: &str, me: &str, members: usize) -> Option<Inbound> {
    if event["type"].as_str() != Some("m.room.message") {
        return None;
    }
    let sender = text(&event["sender"])?;
    let content = &event["content"];
    let relation = &content["m.relates_to"];
    if sender == me || relation["rel_type"].as_str() == Some("m.replace") {
        return None;
    }
    let attachment = media(content);
    let body = content["body"].as_str().unwrap_or_default();
    // A file's body is its name, or its caption when `filename` names the
    // file (Matrix 1.10 media captions).
    let caption = content["filename"]
        .as_str()
        .is_some_and(|name| !name.is_empty() && name != body);
    let body = if attachment.is_none() || caption {
        body.to_owned()
    } else {
        String::new()
    };
    let thread = (relation["rel_type"].as_str() == Some("m.thread"))
        .then(|| text(&relation["event_id"]))
        .flatten();
    let millis = event["origin_server_ts"].as_i64().unwrap_or(0);
    Some(Inbound {
        message_id: text(&event["event_id"])?,
        sender_id: sender,
        sender_name: None,
        chat_id: room.to_owned(),
        chat_type: if members == 2 {
            ChatType::Dm
        } else {
            ChatType::Group
        },
        scope_id: None,
        thread_id: thread,
        timestamp: Timestamp::from_millisecond(millis).unwrap_or_else(|_| Timestamp::now()),
        text: body,
        attachments: attachment.into_iter().collect(),
        reply_to: text(&relation["m.in_reply_to"]["event_id"]),
        is_bot: false,
        callback: None,
        interaction: None,
        profile: None,
        addressing: addressing(content, me),
    })
}

/// A room's joined member count, from the cache or the homeserver.
pub(crate) async fn members(api: &Api, cursor: &Mutex<Cursor>, room: &str) -> usize {
    if let Some(count) = cursor.lock().expect("cursor poisoned").members.get(room) {
        return *count;
    }
    let path = format!("/rooms/{}/joined_members", segment(room));
    let count = api
        .get(&path)
        .await
        .ok()
        .and_then(|m| m["joined"].as_object().map(serde_json::Map::len))
        .unwrap_or(0);
    cursor
        .lock()
        .expect("cursor poisoned")
        .members
        .insert(room.to_owned(), count);
    count
}

/// The error a failed `/sync` stands for.
pub(crate) fn sync_error(err: &SendError) -> (AdapterState, AdapterError) {
    if api::unauthorized(err) {
        return (
            AdapterState::Fatal,
            AdapterError {
                retryable: false,
                code: "unauthorized".to_owned(),
                message: "the homeserver rejected the access token".to_owned(),
            },
        );
    }
    (
        AdapterState::Retrying,
        AdapterError {
            retryable: true,
            code: "matrix_unreachable".to_owned(),
            message: format!("Matrix: {}", err.describe()),
        },
    )
}

impl Run {
    /// Takes the current cursor without answering history (first connect
    /// only).
    pub(crate) async fn start_cursor(&self) -> Result<(), SendError> {
        if self.cursor.lock().expect("cursor poisoned").since.is_some() {
            return Ok(());
        }
        let filter = segment(&json!({"room": {"timeline": {"limit": 1}}}).to_string());
        let synced = self
            .api
            .get(&format!("/sync?timeout=0&filter={filter}"))
            .await?;
        self.cursor.lock().expect("cursor poisoned").since = text(&synced["next_batch"]);
        Ok(())
    }

    /// Polls `/sync` until stopped or the connection fails.
    pub(crate) async fn poll(self) {
        loop {
            let since = self.cursor.lock().expect("cursor poisoned").since.clone();
            let path = format!(
                "/sync?timeout={}&since={}",
                POLL_SECS * 1000,
                segment(&since.unwrap_or_default())
            );
            let wait = Duration::from_secs(POLL_SECS + 10);
            let synced = tokio::select! {
                () = self.stop.cancelled() => return,
                synced = self.api.get_within(&path, wait) => synced,
            };
            let synced = match synced {
                Ok(synced) => synced,
                Err(err) => {
                    let (state, error) = sync_error(&err);
                    tracing::warn!(
                        platform = crate::PLATFORM,
                        "sync stopped: {}",
                        error.message
                    );
                    self.link.report(state, Some(error));
                    return;
                }
            };
            self.keys(&synced).await;
            self.joined(&synced["rooms"]["join"]).await;
            self.invited(&synced["rooms"]["invite"]).await;
            // The cursor moves only after the batch was handed over.
            self.cursor.lock().expect("cursor poisoned").since = text(&synced["next_batch"]);
        }
    }

    async fn joined(&self, rooms: &Value) {
        for (room, data) in rooms.as_object().into_iter().flatten() {
            self.note_encryption(room, data);
            if let Some(count) = data["summary"]["m.joined_member_count"].as_u64() {
                let count = usize::try_from(count).unwrap_or(usize::MAX);
                self.cursor
                    .lock()
                    .expect("cursor poisoned")
                    .members
                    .insert(room.clone(), count);
            }
            for event in data["timeline"]["events"].as_array().into_iter().flatten() {
                if event["type"].as_str() == Some("m.room.member") {
                    // Membership changed: count again when next needed.
                    self.cursor
                        .lock()
                        .expect("cursor poisoned")
                        .members
                        .remove(room);
                }
                match event["type"].as_str() {
                    Some("m.room.message") => self.room_message(room, event).await,
                    Some("m.room.encrypted") => self.encrypted_message(room, event).await,
                    _ => {}
                }
            }
        }
    }

    /// Takes `/sync`'s room keys and tops up the one-time keys.
    async fn keys(&self, synced: &Value) {
        let Some(crypto) = &self.crypto else { return };
        crypto
            .receive_to_device(&synced["to_device"]["events"])
            .await;
        if let Some(count) = synced["device_one_time_keys_count"]["signed_curve25519"].as_u64()
            && let Err(why) = crypto.top_up(count).await
        {
            tracing::warn!(platform = crate::PLATFORM, "{why}");
        }
    }

    /// Notes the rooms whose state or timeline turns encryption on.
    fn note_encryption(&self, room: &str, data: &Value) {
        let turned_on = ["state", "timeline"].iter().any(|part| {
            data[part]["events"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|e| e["type"] == "m.room.encryption")
        });
        if turned_on {
            self.encrypted
                .lock()
                .expect("encrypted poisoned")
                .insert(room.to_owned(), true);
        }
    }

    /// Decrypts an encrypted room event and handles it as a message; one
    /// that cannot be decrypted is dropped (one WARN per room), never
    /// passed on as ciphertext.
    async fn encrypted_message(&self, room: &str, event: &Value) {
        let decrypted = match &self.crypto {
            Some(crypto) => crypto.decrypt(room, event),
            None => Err("end-to-end encryption is not running (options.e2ee)".to_owned()),
        };
        match decrypted {
            Ok(clear) if clear["type"] == "m.room.message" => self.room_message(room, &clear).await,
            Ok(_) => {}
            Err(why) => {
                let first = self
                    .undecryptable
                    .lock()
                    .expect("undecryptable poisoned")
                    .insert(room.to_owned());
                if first {
                    tracing::warn!(
                        platform = crate::PLATFORM,
                        room,
                        "an encrypted event was dropped: {why} (warned once per room)"
                    );
                }
            }
        }
    }

    /// Submits a room message the room rules admit (allowed room, not
    /// stale), scoped to its session.
    async fn room_message(&self, room: &str, event: &Value) {
        let count = members(&self.api, &self.cursor, room).await;
        if !self.rooms.reads(room, count) {
            return;
        }
        let Some(inbound) = message(event, room, &self.user_id, count) else {
            return;
        };
        if self.rooms.stale(inbound.timestamp, Timestamp::now()) {
            return;
        }
        self.link.submit(self.rooms.scoped(inbound)).await;
    }

    /// Joins rooms an allowed user invited the bot to.
    async fn invited(&self, rooms: &Value) {
        for (room, data) in rooms.as_object().into_iter().flatten() {
            let inviter = data["invite_state"]["events"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|e| {
                    e["type"].as_str() == Some("m.room.member")
                        && e["state_key"].as_str() == Some(self.user_id.as_str())
                })
                .and_then(|e| e["sender"].as_str());
            let Some(inviter) = inviter else { continue };
            if !self.rooms.joins(inviter, &self.allow_users) {
                tracing::info!(
                    platform = crate::PLATFORM,
                    room,
                    "ignored an invite (options.auto_join)"
                );
                continue;
            }
            if let Err(err) = self
                .api
                .post(&format!("/join/{}", segment(room)), &json!({}))
                .await
            {
                tracing::warn!(
                    platform = crate::PLATFORM,
                    room,
                    "cannot join: {}",
                    err.describe()
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_two_member_room_message_is_a_dm() {
        let event = json!({"type": "m.room.message", "event_id": "$1", "sender": "@u:hs",
            "origin_server_ts": 1_700_000_000_000_i64,
            "content": {"msgtype": "m.text", "body": "hi"}});
        let inbound = message(&event, "!r:hs", "@bot:hs", 2).unwrap();
        assert_eq!(inbound.chat_type, ChatType::Dm);
        assert_eq!(inbound.message_id, "$1");
        assert_eq!(inbound.text, "hi");
        assert_eq!(
            message(&event, "!r:hs", "@bot:hs", 5).unwrap().chat_type,
            ChatType::Group
        );
    }

    #[test]
    fn own_events_and_edits_are_not_inbound() {
        let own = json!({"type": "m.room.message", "event_id": "$1", "sender": "@bot:hs",
            "content": {"msgtype": "m.text", "body": "x"}});
        let edit = json!({"type": "m.room.message", "event_id": "$2", "sender": "@u:hs",
            "content": {"msgtype": "m.text", "body": "* x",
                        "m.relates_to": {"rel_type": "m.replace", "event_id": "$0"}}});
        assert_eq!(message(&own, "!r:hs", "@bot:hs", 2), None);
        assert_eq!(message(&edit, "!r:hs", "@bot:hs", 2), None);
    }

    #[test]
    fn a_file_with_a_filename_carries_its_body_as_caption() {
        let event = json!({"type": "m.room.message", "event_id": "$5", "sender": "@u:hs",
            "content": {"msgtype": "m.image", "body": "look at this", "filename": "cat.png",
                        "url": "mxc://hs/abc"}});
        let inbound = message(&event, "!r:hs", "@bot:hs", 2).unwrap();
        assert_eq!(inbound.text, "look at this");
        assert_eq!(inbound.attachments[0].name.as_deref(), Some("cat.png"));
        let plain = json!({"type": "m.room.message", "event_id": "$6", "sender": "@u:hs",
            "content": {"msgtype": "m.image", "body": "cat.png", "url": "mxc://hs/abc"}});
        assert_eq!(message(&plain, "!r:hs", "@bot:hs", 2).unwrap().text, "");
    }

    #[test]
    fn threads_replies_and_media_are_kept() {
        let event = json!({"type": "m.room.message", "event_id": "$3", "sender": "@u:hs",
            "content": {"msgtype": "m.image", "body": "cat.png", "url": "mxc://hs/abc",
                        "m.relates_to": {"rel_type": "m.thread", "event_id": "$root",
                                         "m.in_reply_to": {"event_id": "$2"}}}});
        let inbound = message(&event, "!r:hs", "@bot:hs", 3).unwrap();
        assert_eq!(inbound.thread_id.as_deref(), Some("$root"));
        assert_eq!(inbound.reply_to.as_deref(), Some("$2"));
        assert_eq!(inbound.attachments[0].kind, MediaKind::Image);
        assert_eq!(inbound.attachments[0].location, "mxc://hs/abc");
        assert_eq!(inbound.attachments[0].name.as_deref(), Some("cat.png"));
    }
}

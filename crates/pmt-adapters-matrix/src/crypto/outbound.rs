//! Sending into an encrypted room: the room's Megolm session (rotated
//! after 100 messages, a week, or when a device it was shared with left)
//! is shared over Olm with every member device that does not hold it yet
//! — devices whose keys carry a valid self-signature, reached through a
//! signed one-time key — then the event is encrypted with it.

use std::collections::BTreeMap;

use pmt_gateway::messaging::SendError;
use serde_json::{Value, json};
use vodozemac::megolm::{GroupSession, GroupSessionPickle, SessionConfig as MegolmConfig};
use vodozemac::olm::SessionConfig;
use vodozemac::{Curve25519PublicKey, Ed25519PublicKey, base64_encode};

use super::{Crypto, MEGOLM, OLM, signed, store, unpickle};
use crate::api::segment;

/// To-device messages by user, then device.
type ToDevice = BTreeMap<String, serde_json::Map<String, Value>>;

/// Messages a Megolm session encrypts before it is replaced.
const ROTATE_MESSAGES: u32 = 100;
/// Its lifetime (a week, in milliseconds).
const ROTATE_MS: i64 = 7 * 24 * 3600 * 1000;

/// The Megolm session this device sends a room's events with.
pub(super) struct OutboundRoom {
    session: GroupSession,
    shared: Vec<(String, String)>,
    messages: u32,
    created_ms: i64,
}

impl OutboundRoom {
    fn new() -> Self {
        Self {
            session: GroupSession::new(MegolmConfig::version_1()),
            shared: Vec::new(),
            messages: 0,
            created_ms: jiff::Timestamp::now().as_millisecond(),
        }
    }

    pub(super) fn restore(entry: &store::Outbound, key: &[u8; 32]) -> Result<Self, String> {
        let pickle = unpickle(
            "outbound Megolm session",
            GroupSessionPickle::from_encrypted(&entry.pickle, key),
        )?;
        Ok(Self {
            session: GroupSession::from_pickle(pickle),
            shared: entry.shared.clone(),
            messages: entry.messages,
            created_ms: entry.created_ms,
        })
    }

    pub(super) fn persist(&self, key: &[u8; 32]) -> store::Outbound {
        store::Outbound {
            pickle: self.session.pickle().encrypt(key),
            shared: self.shared.clone(),
            messages: self.messages,
            created_ms: self.created_ms,
        }
    }

    /// Whether it must be replaced before sending to `devices`.
    fn expired(&self, devices: &[Member]) -> bool {
        let left = self
            .shared
            .iter()
            .any(|(u, d)| !devices.iter().any(|x| &x.user == u && &x.device == d));
        let age = jiff::Timestamp::now().as_millisecond() - self.created_ms;
        left || self.messages >= ROTATE_MESSAGES || age >= ROTATE_MS
    }
}

/// A member's device with verified keys.
#[derive(Debug, Clone)]
pub(super) struct Member {
    user: String,
    device: String,
    curve: Curve25519PublicKey,
    ed: Ed25519PublicKey,
}

/// One device of a `keys/query` answer when its keys carry a valid
/// self-signature.
fn verified(user: &str, device: &str, keys: &Value) -> Option<Member> {
    if keys["user_id"] != user || keys["device_id"] != device {
        return None;
    }
    let key = |kind: &str| keys["keys"][format!("{kind}:{device}")].as_str();
    let curve = Curve25519PublicKey::from_base64(key("curve25519")?).ok()?;
    let ed = Ed25519PublicKey::from_base64(key("ed25519")?).ok()?;
    if !signed::verify(keys, user, &format!("ed25519:{device}"), &ed) {
        tracing::warn!(platform = crate::PLATFORM, %user, %device, "a device's keys are not self-signed; it gets no room key");
        return None;
    }
    Some(Member {
        user: user.to_owned(),
        device: device.to_owned(),
        curve,
        ed,
    })
}

/// The verified devices of a `keys/query` answer but `skip` (this one).
fn devices_of(answer: &Value, skip: (&str, &str)) -> Vec<Member> {
    let mut found = Vec::new();
    for (user, devices) in answer["device_keys"].as_object().into_iter().flatten() {
        for (device, keys) in devices.as_object().into_iter().flatten() {
            if (user.as_str(), device.as_str()) != skip
                && let Some(member) = verified(user, device, keys)
            {
                found.push(member);
            }
        }
    }
    found
}

fn send_error(what: &str, err: &SendError) -> SendError {
    match err {
        SendError::Permanent(why) => SendError::Permanent(format!("{what}: {why}")),
        other => other.clone(),
    }
}

impl Crypto {
    /// `content` of type `kind` encrypted for `room`, whose members are
    /// `members`, as an `m.room.encrypted` content.
    ///
    /// # Errors
    ///
    /// A key request failed.
    pub(crate) async fn encrypt(
        &self,
        room: &str,
        kind: &str,
        content: &Value,
        members: &[String],
    ) -> Result<Value, SendError> {
        let query = json!({"device_keys": members.iter().map(|m| (m.clone(), json!([]))).collect::<serde_json::Map<_, _>>()});
        let answer = self
            .api
            .post("/keys/query", &query)
            .await
            .map_err(|e| send_error("keys/query", &e))?;
        let devices = devices_of(&answer, (&self.user, &self.device));
        let missing: Vec<Member> = {
            let mut state = self.lock();
            if state
                .outbound
                .get(room)
                .is_some_and(|o| o.expired(&devices))
            {
                state.outbound.remove(room);
            }
            let outbound = state
                .outbound
                .entry(room.to_owned())
                .or_insert_with(OutboundRoom::new);
            devices
                .into_iter()
                .filter(|d| {
                    !outbound
                        .shared
                        .contains(&(d.user.clone(), d.device.clone()))
                })
                .collect()
        };
        if !missing.is_empty() {
            self.share(room, &missing).await?;
        }
        let plaintext = json!({"type": kind, "content": content, "room_id": room});
        let encrypted = {
            let mut state = self.lock();
            let sender_key = state.account.curve25519_key().to_base64();
            let outbound = state.outbound.get_mut(room).expect("created above");
            let message = outbound.session.encrypt(plaintext.to_string());
            outbound.messages += 1;
            json!({
                "algorithm": MEGOLM,
                "sender_key": sender_key,
                "ciphertext": message.to_base64(),
                "session_id": outbound.session.session_id(),
                "device_id": self.device,
            })
        };
        self.save_or_warn();
        Ok(encrypted)
    }

    /// Shares the room's Megolm session with `devices` over Olm, opening
    /// sessions through their one-time keys where none exists.
    async fn share(&self, room: &str, devices: &[Member]) -> Result<(), SendError> {
        let without: Vec<&Member> = {
            let state = self.lock();
            devices
                .iter()
                .filter(|d| {
                    state
                        .sessions
                        .get(&d.curve.to_base64())
                        .is_none_or(Vec::is_empty)
                })
                .collect()
        };
        if !without.is_empty() {
            self.open_sessions(&without).await?;
        }
        let (messages, reached) = self.room_key_messages(room, devices);
        if reached.is_empty() {
            return Ok(());
        }
        let txn = format!("pmtkey{}", jiff::Timestamp::now().as_nanosecond());
        let path = format!("/sendToDevice/m.room.encrypted/{}", segment(&txn));
        self.api
            .put(&path, &json!({"messages": messages}))
            .await
            .map_err(|e| send_error("sendToDevice", &e))?;
        if let Some(outbound) = self.lock().outbound.get_mut(room) {
            outbound.shared.extend(reached);
        }
        self.save_or_warn();
        Ok(())
    }

    /// Claims a signed one-time key of each device and opens an Olm
    /// session with it.
    async fn open_sessions(&self, devices: &[&Member]) -> Result<(), SendError> {
        let mut wanted: BTreeMap<String, serde_json::Map<String, Value>> = BTreeMap::new();
        for d in devices {
            wanted
                .entry(d.user.clone())
                .or_default()
                .insert(d.device.clone(), json!("signed_curve25519"));
        }
        let claimed = self
            .api
            .post("/keys/claim", &json!({"one_time_keys": wanted}))
            .await
            .map_err(|e| send_error("keys/claim", &e))?;
        let mut state = self.lock();
        for d in devices {
            let keys = &claimed["one_time_keys"][&d.user][&d.device];
            let found = keys.as_object().and_then(|m| m.values().next());
            let key = found
                .filter(|k| signed::verify(k, &d.user, &format!("ed25519:{}", d.device), &d.ed));
            let Some(key) =
                key.and_then(|k| Curve25519PublicKey::from_base64(k["key"].as_str()?).ok())
            else {
                tracing::warn!(platform = crate::PLATFORM, user = %d.user, device = %d.device, "no signed one-time key; the device gets no room key");
                continue;
            };
            match state
                .account
                .create_outbound_session(SessionConfig::version_1(), d.curve, key)
            {
                Ok(session) => state
                    .sessions
                    .entry(d.curve.to_base64())
                    .or_default()
                    .push(session),
                Err(e) => tracing::warn!(platform = crate::PLATFORM, "no Olm session: {e}"),
            }
        }
        Ok(())
    }

    /// The to-device messages carrying the room key to each device with an
    /// Olm session, and the devices they reach.
    fn room_key_messages(
        &self,
        room: &str,
        devices: &[Member],
    ) -> (ToDevice, Vec<(String, String)>) {
        let mut state = self.lock();
        let ours = state.account.identity_keys();
        let Some(room_key) = state.outbound.get(room).map(|outbound| {
            json!({
                "algorithm": MEGOLM,
                "room_id": room,
                "session_id": outbound.session.session_id(),
                "session_key": outbound.session.session_key().to_base64(),
            })
        }) else {
            return (BTreeMap::new(), Vec::new());
        };
        let mut messages: BTreeMap<String, serde_json::Map<String, Value>> = BTreeMap::new();
        let mut reached = Vec::new();
        for d in devices {
            let Some(session) = state
                .sessions
                .get_mut(&d.curve.to_base64())
                .and_then(|s| s.last_mut())
            else {
                continue;
            };
            let payload = json!({
                "type": "m.room_key",
                "content": room_key,
                "sender": self.user,
                "sender_device": self.device,
                "keys": {"ed25519": ours.ed25519.to_base64()},
                "recipient": d.user,
                "recipient_keys": {"ed25519": d.ed.to_base64()},
            });
            let Ok(message) = session.encrypt(payload.to_string()) else {
                continue;
            };
            let (kind, body) = message.to_parts();
            let content = json!({
                "algorithm": OLM,
                "sender_key": ours.curve25519.to_base64(),
                "ciphertext": {d.curve.to_base64(): {"type": kind, "body": base64_encode(body)}},
            });
            messages
                .entry(d.user.clone())
                .or_default()
                .insert(d.device.clone(), content);
            reached.push((d.user.clone(), d.device.clone()));
        }
        (messages, reached)
    }
}

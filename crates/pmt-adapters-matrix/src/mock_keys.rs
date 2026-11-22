//! The end-to-end encryption half of the Matrix mock: the homeserver's
//! key endpoints (`keys/upload|query|claim`, `sendToDevice`, signature
//! uploads, account data for secret storage) and [`Device`], another
//! user's client built on vodozemac that sends encrypted messages to the
//! bot and reads what the bot encrypts for it.

// A fixture: a mock that cannot serve should fail loudly.
#![allow(clippy::missing_panics_doc)]

use std::collections::BTreeMap;
use std::sync::Mutex;

use pmt_gateway::messaging::HttpResponse;
use pmt_gateway::messaging::mock_server;
use serde_json::{Value, json};
use vodozemac::megolm::{
    GroupSession, InboundGroupSession, MegolmMessage, SessionConfig as MegolmConfig,
};
use vodozemac::olm::{Account, OlmMessage, Session, SessionConfig};
use vodozemac::{Curve25519PublicKey, Ed25519SecretKey, base64_decode, base64_encode};

use crate::crypto::{MEGOLM, OLM, recovery, signed};

type DeviceId = (String, String);

#[derive(Default)]
struct State {
    devices: BTreeMap<DeviceId, Value>,
    /// Published one-time keys, oldest first: (key id, signed key).
    otks: BTreeMap<DeviceId, Vec<(String, Value)>>,
    /// To-device events waiting for their device.
    to_device: Vec<(DeviceId, Value)>,
    account_data: BTreeMap<(String, String), Value>,
    self_signing: BTreeMap<String, Value>,
    signatures: Vec<Value>,
}

/// The homeserver's key store.
#[derive(Default)]
pub struct Keys {
    state: Mutex<State>,
}

fn id(user: &str, device: &str) -> DeviceId {
    (user.to_owned(), device.to_owned())
}

impl Keys {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    /// Serves a key endpoint for `who` (the token's device); `None` for
    /// other paths.
    pub(crate) fn serve(
        &self,
        who: (&str, &str),
        method: &str,
        parts: &[&str],
        body: &Value,
    ) -> Option<HttpResponse> {
        Some(match (method, parts) {
            ("POST", ["keys", "upload"]) => self.upload(who, body),
            ("POST", ["keys", "query"]) => self.query(body),
            ("POST", ["keys", "claim"]) => self.claim(body),
            ("PUT", ["sendToDevice", kind, _]) => self.send_to_device(who.0, kind, body),
            ("POST", ["keys", "signatures", "upload"]) => {
                self.lock().signatures.push(body.clone());
                mock_server::json(200, &json!({"failures": {}}))
            }
            ("GET", ["user", user, "account_data", kind]) => {
                let found = self
                    .lock()
                    .account_data
                    .get(&((*user).to_owned(), (*kind).to_owned()))
                    .cloned();
                found.map_or_else(
                    || {
                        mock_server::json(
                            404,
                            &json!({"errcode": "M_NOT_FOUND", "error": "M_NOT_FOUND"}),
                        )
                    },
                    |v| mock_server::json(200, &v),
                )
            }
            _ => return None,
        })
    }

    fn upload(&self, who: (&str, &str), body: &Value) -> HttpResponse {
        let mut state = self.lock();
        let device = id(who.0, who.1);
        if body["device_keys"].is_object() {
            state
                .devices
                .insert(device.clone(), body["device_keys"].clone());
        }
        let stored = state.otks.entry(device).or_default();
        for (key_id, key) in body["one_time_keys"].as_object().into_iter().flatten() {
            match stored.iter().find(|(k, _)| k == key_id) {
                Some((_, old)) if old != key => {
                    return mock_server::json(
                        400,
                        &json!({"errcode": "M_INVALID_PARAM",
                                "error": format!("One time key {key_id} already exists")}),
                    );
                }
                Some(_) => {}
                None => stored.push((key_id.clone(), key.clone())),
            }
        }
        let count = stored.len();
        mock_server::json(
            200,
            &json!({"one_time_key_counts": {"signed_curve25519": count}}),
        )
    }

    fn query(&self, body: &Value) -> HttpResponse {
        let state = self.lock();
        let mut device_keys = serde_json::Map::new();
        let mut self_signing = serde_json::Map::new();
        for user in body["device_keys"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(u, _)| u)
        {
            let devices: serde_json::Map<String, Value> = state
                .devices
                .iter()
                .filter(|((u, _), _)| u == user)
                .map(|((_, d), keys)| (d.clone(), keys.clone()))
                .collect();
            device_keys.insert(user.clone(), Value::Object(devices));
            if let Some(key) = state.self_signing.get(user) {
                self_signing.insert(user.clone(), key.clone());
            }
        }
        mock_server::json(
            200,
            &json!({"device_keys": device_keys, "self_signing_keys": self_signing}),
        )
    }

    fn claim(&self, body: &Value) -> HttpResponse {
        let mut state = self.lock();
        let mut out = serde_json::Map::new();
        for (user, devices) in body["one_time_keys"].as_object().into_iter().flatten() {
            let mut claimed = serde_json::Map::new();
            for (device, _) in devices.as_object().into_iter().flatten() {
                let keys = state.otks.entry(id(user, device)).or_default();
                if !keys.is_empty() {
                    let (key_id, key) = keys.remove(0);
                    claimed.insert(device.clone(), json!({key_id: key}));
                }
            }
            out.insert(user.clone(), Value::Object(claimed));
        }
        mock_server::json(200, &json!({"one_time_keys": out}))
    }

    fn send_to_device(&self, sender: &str, kind: &str, body: &Value) -> HttpResponse {
        let mut state = self.lock();
        for (user, devices) in body["messages"].as_object().into_iter().flatten() {
            for (device, content) in devices.as_object().into_iter().flatten() {
                let event = json!({"type": kind, "sender": sender, "content": content});
                state.to_device.push((id(user, device), event));
            }
        }
        mock_server::json(200, &json!({}))
    }

    /// Takes the to-device events waiting for `user`'s `device`.
    pub fn take_to_device(&self, user: &str, device: &str) -> Vec<Value> {
        let mut state = self.lock();
        let wanted = id(user, device);
        let (mine, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut state.to_device)
            .into_iter()
            .partition(|(d, _)| *d == wanted);
        state.to_device = rest;
        mine.into_iter().map(|(_, e)| e).collect()
    }

    /// Whether to-device events wait for `user`'s `device`.
    pub fn has_to_device(&self, user: &str, device: &str) -> bool {
        let wanted = id(user, device);
        self.lock().to_device.iter().any(|(d, _)| *d == wanted)
    }

    /// The one-time keys `user`'s `device` has published and not had
    /// claimed.
    pub fn one_time_key_count(&self, user: &str, device: &str) -> usize {
        self.lock().otks.get(&id(user, device)).map_or(0, Vec::len)
    }

    /// Someone claims the `n` oldest one-time keys of `user`'s `device`.
    pub fn claim_oldest(&self, user: &str, device: &str, n: usize) {
        let mut state = self.lock();
        let keys = state.otks.entry(id(user, device)).or_default();
        let n = n.min(keys.len());
        keys.drain(..n);
    }

    /// The device keys `user`'s `device` published.
    pub fn device_keys(&self, user: &str, device: &str) -> Option<Value> {
        self.lock().devices.get(&id(user, device)).cloned()
    }

    /// Signature uploads received.
    pub fn signature_uploads(&self) -> Vec<Value> {
        self.lock().signatures.clone()
    }

    /// Sets up `user`'s cross-signing with secret storage under
    /// `recovery_key`: a self-signing key (published, and stored encrypted
    /// in account data); its public key.
    pub fn cross_signing(&self, user: &str, recovery_key: &[u8; 32]) -> String {
        let ssk = Ed25519SecretKey::new();
        let public = ssk.public_key().to_base64();
        let seed = base64_encode(ssk.to_bytes().as_slice());
        let mut state = self.lock();
        let key_id = "kit-key";
        let mut data = |kind: &str, value: Value| {
            state
                .account_data
                .insert((user.to_owned(), kind.to_owned()), value);
        };
        data("m.secret_storage.default_key", json!({"key": key_id}));
        data(
            &format!("m.secret_storage.key.{key_id}"),
            recovery::key_description(recovery_key, [5; 16]),
        );
        data(
            "m.cross_signing.self_signing",
            json!({"encrypted": {key_id: recovery::encrypt_secret(
                recovery_key, "m.cross_signing.self_signing", seed.as_bytes(), [6; 16])}}),
        );
        state.self_signing.insert(
            user.to_owned(),
            json!({"user_id": user, "usage": ["self_signing"],
                   "keys": {format!("ed25519:{public}"): public}}),
        );
        public
    }
}

/// Whether `object` carries `user`'s valid signature by the Ed25519 key
/// `key` (base64), under `ed25519:<key_id>`.
#[must_use]
pub fn verify_signed(object: &Value, user: &str, key_id: &str, key: &str) -> bool {
    vodozemac::Ed25519PublicKey::from_base64(key)
        .is_ok_and(|key| signed::verify(object, user, &format!("ed25519:{key_id}"), &key))
}

/// `plain` as an encrypted attachment: the ciphertext to store and its
/// `file` object (without `url`).
#[must_use]
pub fn seal_attachment(plain: &[u8]) -> (Vec<u8>, Value) {
    crate::crypto::attachment::seal(plain).unwrap()
}

/// A recovery key's text, as Element shows it.
#[must_use]
pub fn recovery_key_text(key: &[u8; 32]) -> String {
    recovery::encode_recovery_key(key)
}

/// Another user's client device: it publishes its keys, sends the bot
/// Megolm-encrypted messages (sharing the room key over Olm) and decrypts
/// what the bot encrypts for it.
pub struct Device {
    user: String,
    id: String,
    account: Account,
    sessions: Vec<Session>,
    outbound: GroupSession,
    inbound: BTreeMap<String, InboundGroupSession>,
}

impl Device {
    /// A new device of `user`, its keys and 5 one-time keys published.
    pub fn publish(keys: &Keys, user: &str, device: &str) -> Self {
        let mut account = Account::new();
        let identity = account.identity_keys();
        let mut device_keys = json!({
            "user_id": user, "device_id": device, "algorithms": [OLM, MEGOLM],
            "keys": {format!("curve25519:{device}"): identity.curve25519.to_base64(),
                     format!("ed25519:{device}"): identity.ed25519.to_base64()},
        });
        let key_id = format!("ed25519:{device}");
        let signature = account.sign(signed::signable(&device_keys)).to_base64();
        signed::add_signature(&mut device_keys, user, &key_id, &signature);
        account.generate_one_time_keys(5);
        let mut otks = serde_json::Map::new();
        for (id, key) in account.one_time_keys() {
            let mut object = json!({"key": key.to_base64()});
            let signature = account.sign(signed::signable(&object)).to_base64();
            signed::add_signature(&mut object, user, &key_id, &signature);
            otks.insert(format!("signed_curve25519:{}", id.to_base64()), object);
        }
        account.mark_keys_as_published();
        keys.upload(
            (user, device),
            &json!({"device_keys": device_keys, "one_time_keys": otks}),
        );
        Self {
            user: user.to_owned(),
            id: device.to_owned(),
            account,
            sessions: Vec::new(),
            outbound: GroupSession::new(MegolmConfig::version_1()),
            inbound: BTreeMap::new(),
        }
    }

    /// Opens an Olm session with `user`'s `device` (claiming one of its
    /// one-time keys) and sends it this device's room key for `room`.
    pub fn share_room_key(&mut self, keys: &Keys, room: &str, user: &str, device: &str) {
        let their = keys
            .device_keys(user, device)
            .expect("the device published its keys");
        let curve = Curve25519PublicKey::from_base64(
            their["keys"][format!("curve25519:{device}")]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        let claimed = keys.claim(&json!({"one_time_keys": {user: {device: "signed_curve25519"}}}));
        let claimed: Value = serde_json::from_slice(&claimed.body).unwrap();
        let otk = claimed["one_time_keys"][user][device]
            .as_object()
            .and_then(|m| m.values().next())
            .expect("a one-time key")["key"]
            .as_str()
            .unwrap()
            .to_owned();
        let mut session = self
            .account
            .create_outbound_session(
                SessionConfig::version_1(),
                curve,
                Curve25519PublicKey::from_base64(&otk).unwrap(),
            )
            .unwrap();
        let payload = json!({
            "type": "m.room_key",
            "content": {"algorithm": MEGOLM, "room_id": room,
                        "session_id": self.outbound.session_id(),
                        "session_key": self.outbound.session_key().to_base64()},
            "sender": self.user, "sender_device": self.id,
            "keys": {"ed25519": self.account.ed25519_key().to_base64()},
            "recipient": user,
            "recipient_keys": {"ed25519": their["keys"][format!("ed25519:{device}")]},
        });
        let (kind, body) = session.encrypt(payload.to_string()).unwrap().to_parts();
        self.sessions.push(session);
        let content = json!({"algorithm": OLM, "sender_key": self.account.curve25519_key().to_base64(),
                             "ciphertext": {curve.to_base64(): {"type": kind, "body": base64_encode(body)}}});
        keys.send_to_device(
            &self.user,
            "m.room.encrypted",
            &json!({"messages": {user: {device: content}}}),
        );
    }

    /// The `m.room.encrypted` content of a text message in `room`.
    pub fn encrypt_text(&mut self, room: &str, text: &str) -> Value {
        self.encrypt(room, &json!({"msgtype": "m.text", "body": text}))
    }

    /// The `m.room.encrypted` content of an `m.room.message` with
    /// `content` in `room`.
    pub fn encrypt(&mut self, room: &str, content: &Value) -> Value {
        let plaintext = json!({"type": "m.room.message", "room_id": room, "content": content});
        let message = self.outbound.encrypt(plaintext.to_string());
        json!({"algorithm": MEGOLM, "sender_key": self.account.curve25519_key().to_base64(),
               "ciphertext": message.to_base64(), "session_id": self.outbound.session_id(),
               "device_id": self.id})
    }

    /// Takes the room keys waiting for this device.
    pub fn receive(&mut self, keys: &Keys) {
        let ours = self.account.curve25519_key().to_base64();
        for event in keys.take_to_device(&self.user, &self.id) {
            let content = &event["content"];
            let entry = &content["ciphertext"][&ours];
            let body = base64_decode(entry["body"].as_str().unwrap()).unwrap();
            let message = OlmMessage::from_parts(
                usize::try_from(entry["type"].as_u64().unwrap()).unwrap(),
                &body,
            )
            .unwrap();
            let sender =
                Curve25519PublicKey::from_base64(content["sender_key"].as_str().unwrap()).unwrap();
            let plaintext = match &message {
                OlmMessage::PreKey(prekey) => {
                    let created = self
                        .account
                        .create_inbound_session(SessionConfig::version_1(), sender, prekey)
                        .unwrap();
                    self.sessions.push(created.session);
                    created.plaintext
                }
                OlmMessage::Normal(_) => self
                    .sessions
                    .iter_mut()
                    .find_map(|s| s.decrypt(&message).ok())
                    .expect("a session decrypts it"),
            };
            let payload: Value = serde_json::from_slice(&plaintext).unwrap();
            let key = &payload["content"];
            let session = InboundGroupSession::new(
                &vodozemac::megolm::SessionKey::from_base64(key["session_key"].as_str().unwrap())
                    .unwrap(),
                MegolmConfig::version_1(),
            );
            self.inbound
                .insert(key["session_id"].as_str().unwrap().to_owned(), session);
        }
    }

    /// The plaintext event of an `m.room.encrypted` content the bot sent;
    /// `None` when this device holds no key for it.
    pub fn decrypt(&mut self, content: &Value) -> Option<Value> {
        let session = self.inbound.get_mut(content["session_id"].as_str()?)?;
        let message = MegolmMessage::from_base64(content["ciphertext"].as_str()?).ok()?;
        let decrypted = session.decrypt(&message).ok()?;
        serde_json::from_slice(&decrypted.plaintext).ok()
    }
}

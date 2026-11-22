//! Matrix end-to-end encryption (spec: platform-adapters, Matrix
//! end-to-end encryption) on vodozemac's Olm and Megolm:
//!
//! - this device's keys are uploaded once (signed with its Ed25519 key)
//!   and its one-time keys kept topped up from `/sync`'s count;
//! - Olm-encrypted to-device events bring the Megolm room keys other
//!   devices share, which decrypt `m.room.encrypted` room events; an event
//!   that cannot be decrypted is dropped (the caller warns once per room),
//!   never passed on as ciphertext;
//! - sending into an encrypted room shares this device's Megolm session
//!   with every member device first ([`outbound`]);
//! - the store is [`store`]'s; secret storage and cross-signing are
//!   [`recovery`]'s.
//!
//! Every way encryption cannot start is an `Err` with the operator's
//! remediation: the adapter then fails closed (`required`) or goes on
//! without encryption (`optional`).

pub(crate) mod attachment;
mod outbound;
pub(crate) mod recovery;
pub(crate) mod signed;
pub(crate) mod store;

use std::collections::BTreeMap;
use std::sync::Mutex;

use pmt_config::ProfileScope;
use serde_json::{Value, json};
use vodozemac::megolm::{
    InboundGroupSession, InboundGroupSessionPickle, MegolmMessage, SessionKey,
};
use vodozemac::olm::{Account, AccountPickle, OlmMessage, Session, SessionConfig, SessionPickle};
use vodozemac::{Curve25519PublicKey, base64_decode};

use crate::api::Api;
use outbound::OutboundRoom;

/// The Olm algorithm of to-device messages.
pub(crate) const OLM: &str = "m.olm.v1.curve25519-aes-sha2";
/// The Megolm algorithm of room messages.
pub(crate) const MEGOLM: &str = "m.megolm.v1.aes-sha2";

/// The live crypto state (what the store persists).
struct State {
    account: Account,
    uploaded: bool,
    /// Olm sessions by the other device's Curve25519 key.
    sessions: BTreeMap<String, Vec<Session>>,
    /// Megolm sessions others share, by `<room> <session id>`.
    inbound: BTreeMap<String, (store::Inbound, InboundGroupSession)>,
    outbound: BTreeMap<String, OutboundRoom>,
}

fn unpickle<T>(what: &str, result: Result<T, vodozemac::PickleError>) -> Result<T, String> {
    result.map_err(|e| format!("the crypto store is corrupted ({what}: {e})"))
}

impl State {
    fn new() -> Self {
        Self {
            account: Account::new(),
            uploaded: false,
            sessions: BTreeMap::new(),
            inbound: BTreeMap::new(),
            outbound: BTreeMap::new(),
        }
    }

    fn restore(p: &store::Persisted, key: &[u8; 32]) -> Result<Self, String> {
        let account = Account::from_pickle(unpickle(
            "account",
            AccountPickle::from_encrypted(&p.account, key),
        )?);
        let mut sessions = BTreeMap::new();
        for (sender, pickles) in &p.sessions {
            let mut list = Vec::new();
            for pickle in pickles {
                let pickle = unpickle("Olm session", SessionPickle::from_encrypted(pickle, key))?;
                list.push(Session::from_pickle(pickle));
            }
            sessions.insert(sender.clone(), list);
        }
        let mut inbound = BTreeMap::new();
        for (id, entry) in &p.inbound {
            let pickle = unpickle(
                "Megolm session",
                InboundGroupSessionPickle::from_encrypted(&entry.pickle, key),
            )?;
            inbound.insert(
                id.clone(),
                (entry.clone(), InboundGroupSession::from_pickle(pickle)),
            );
        }
        let mut outbound = BTreeMap::new();
        for (room, entry) in &p.outbound {
            outbound.insert(room.clone(), OutboundRoom::restore(entry, key)?);
        }
        Ok(Self {
            account,
            uploaded: p.uploaded,
            sessions,
            inbound,
            outbound,
        })
    }

    fn persist(&self, user: &str, device: &str, key: &[u8; 32]) -> store::Persisted {
        store::Persisted {
            user_id: user.to_owned(),
            device_id: device.to_owned(),
            account: self.account.pickle().encrypt(key),
            uploaded: self.uploaded,
            sessions: self
                .sessions
                .iter()
                .map(|(k, list)| {
                    (
                        k.clone(),
                        list.iter().map(|s| s.pickle().encrypt(key)).collect(),
                    )
                })
                .collect(),
            inbound: self
                .inbound
                .iter()
                .map(|(id, (entry, session))| {
                    let mut entry = entry.clone();
                    entry.pickle = session.pickle().encrypt(key);
                    (id.clone(), entry)
                })
                .collect(),
            outbound: self
                .outbound
                .iter()
                .map(|(room, o)| (room.clone(), o.persist(key)))
                .collect(),
        }
    }
}

/// This device's end-to-end encryption.
pub(crate) struct Crypto {
    api: Api,
    user: String,
    device: String,
    store: store::Store,
    state: Mutex<State>,
}

impl std::fmt::Debug for Crypto {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Crypto")
            .field("user", &self.user)
            .field("device", &self.device)
            .finish_non_exhaustive()
    }
}

/// The text of a store whose keys no longer match the homeserver's.
fn mismatch(dir: &std::path::Path, what: &str) -> String {
    format!(
        "{what}: the crypto store {} no longer matches the homeserver (restored from a backup or \
         copied from another install), so encryption is refused. Stop the daemon, delete that \
         directory and the secret {}, then sign in again with a new access token (a new device).",
        dir.display(),
        store::KEY_SECRET
    )
}

impl Crypto {
    /// Starts encryption for `user`'s device `device`: the store opened
    /// (or created), the device's keys published and checked against the
    /// homeserver's, one-time keys topped up, and the device self-signed
    /// when the profile holds `MATRIX_RECOVERY_KEY`.
    ///
    /// # Errors
    ///
    /// Why encryption cannot run, with the remediation.
    pub(crate) async fn start(
        scope: &ProfileScope,
        api: &Api,
        user: &str,
        device: Option<&str>,
    ) -> Result<Self, String> {
        let device = device
            .filter(|d| !d.is_empty())
            .ok_or("the access token has no device id; sign in with a device to use encryption")?;
        let (store, persisted) = store::Store::open(scope)?;
        let state = match persisted {
            Some(p) if p.user_id != user || p.device_id != device => {
                return Err(format!(
                    "the crypto store {} belongs to device {} of {}, not {device}; delete it (and \
                     the secret {}) to start this device's store",
                    store.dir().display(),
                    p.device_id,
                    p.user_id,
                    store::KEY_SECRET
                ));
            }
            Some(p) => State::restore(&p, &store.key)?,
            None => State::new(),
        };
        let crypto = Self {
            api: api.clone(),
            user: user.to_owned(),
            device: device.to_owned(),
            store,
            state: Mutex::new(state),
        };
        crypto.save()?;
        crypto.publish().await?;
        crypto.self_sign(scope).await;
        Ok(crypto)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("crypto poisoned")
    }

    fn save(&self) -> Result<(), String> {
        let persisted = self
            .lock()
            .persist(&self.user, &self.device, &self.store.key);
        self.store.save(&persisted)
    }

    fn save_or_warn(&self) {
        if let Err(why) = self.save() {
            tracing::warn!(platform = crate::PLATFORM, "{why}");
        }
    }

    /// This device's keys, signed with its Ed25519 key.
    fn device_keys(&self) -> Value {
        let state = self.lock();
        let keys = state.account.identity_keys();
        let device = &self.device;
        let mut object = json!({
            "user_id": self.user,
            "device_id": device,
            "algorithms": [OLM, MEGOLM],
            "keys": {
                format!("curve25519:{device}"): keys.curve25519.to_base64(),
                format!("ed25519:{device}"): keys.ed25519.to_base64(),
            },
        });
        let signature = state.account.sign(signed::signable(&object)).to_base64();
        signed::add_signature(
            &mut object,
            &self.user,
            &format!("ed25519:{device}"),
            &signature,
        );
        object
    }

    /// Uploads the device keys (once), refuses a homeserver that holds
    /// other keys for this device, and tops up the one-time keys.
    async fn publish(&self) -> Result<(), String> {
        let uploaded = self.lock().uploaded;
        let body = if uploaded {
            json!({})
        } else {
            json!({"device_keys": self.device_keys()})
        };
        let answer = self
            .api
            .post("/keys/upload", &body)
            .await
            .map_err(|e| format!("keys/upload: {}", e.describe()))?;
        self.lock().uploaded = true;
        self.save()?;
        let query = json!({"device_keys": {&self.user: [&self.device]}});
        let published = self
            .api
            .post("/keys/query", &query)
            .await
            .map_err(|e| format!("keys/query: {}", e.describe()))?;
        let ours = self.lock().account.curve25519_key().to_base64();
        let theirs = &published["device_keys"][&self.user][&self.device]["keys"]
            [format!("curve25519:{}", self.device)];
        if theirs.as_str().is_some_and(|k| k != ours) {
            return Err(mismatch(
                self.store.dir(),
                "the homeserver holds other keys for this device",
            ));
        }
        self.top_up(
            answer["one_time_key_counts"]["signed_curve25519"]
                .as_u64()
                .unwrap_or(0),
        )
        .await
    }

    /// Uploads one-time keys until the homeserver holds half the account's
    /// capacity.
    ///
    /// # Errors
    ///
    /// The homeserver refused them (the store no longer matches it), or the
    /// store cannot be written.
    pub(crate) async fn top_up(&self, published: u64) -> Result<(), String> {
        let keys = {
            let mut state = self.lock();
            let target =
                u64::try_from(state.account.max_number_of_one_time_keys() / 2).unwrap_or(50);
            if published >= target && state.account.one_time_keys().is_empty() {
                return Ok(());
            }
            let missing = usize::try_from(target.saturating_sub(published)).unwrap_or(0);
            if missing > 0 {
                state.account.generate_one_time_keys(missing);
            }
            let mut keys = serde_json::Map::new();
            for (id, key) in state.account.one_time_keys() {
                let mut object = json!({"key": key.to_base64()});
                let signature = state.account.sign(signed::signable(&object)).to_base64();
                signed::add_signature(
                    &mut object,
                    &self.user,
                    &format!("ed25519:{}", self.device),
                    &signature,
                );
                keys.insert(format!("signed_curve25519:{}", id.to_base64()), object);
            }
            keys
        };
        if keys.is_empty() {
            return Ok(());
        }
        // Generated keys are kept before they are published, so a crash
        // between the two uploads the same keys again.
        self.save()?;
        match self
            .api
            .post("/keys/upload", &json!({"one_time_keys": keys}))
            .await
        {
            Ok(_) => {
                self.lock().account.mark_keys_as_published();
                self.save()
            }
            Err(err) if err.describe().contains("already exists") => Err(mismatch(
                self.store.dir(),
                "the homeserver refused this store's one-time keys",
            )),
            Err(err) => Err(format!("keys/upload: {}", err.describe())),
        }
    }

    /// Signs this device with the self-signing key from secret storage
    /// when the profile holds `MATRIX_RECOVERY_KEY` (a failure is logged).
    async fn self_sign(&self, scope: &ProfileScope) {
        let Ok(Some(recovery)) = scope.secrets().store().get(recovery::RECOVERY_SECRET) else {
            return;
        };
        let keys = self.device_keys();
        match recovery::self_sign(
            &self.api,
            &self.user,
            &self.device,
            &keys,
            recovery.expose(),
        )
        .await
        {
            Ok(()) => tracing::info!(
                platform = crate::PLATFORM,
                device = %self.device,
                "the device is signed with the account's self-signing key"
            ),
            Err(why) => tracing::warn!(
                platform = crate::PLATFORM,
                "the device is not cross-signed ({}): {why}",
                recovery::RECOVERY_SECRET
            ),
        }
    }

    /// Takes the room keys of `/sync`'s to-device events.
    pub(crate) fn receive_to_device(&self, events: &Value) {
        let mut changed = false;
        for event in events.as_array().into_iter().flatten() {
            if event["type"] != "m.room.encrypted" || event["content"]["algorithm"] != OLM {
                continue;
            }
            let sender = event["sender"].as_str().unwrap_or_default();
            match self.decrypt_olm(sender, &event["content"]) {
                Ok(payload) => changed |= self.room_key(&payload, &event["content"]),
                Err(why) => tracing::warn!(
                    platform = crate::PLATFORM,
                    sender,
                    "a to-device message was not decrypted: {why}"
                ),
            }
        }
        if changed {
            self.save_or_warn();
        }
    }

    /// The payload of an Olm message to this device.
    fn decrypt_olm(&self, sender: &str, content: &Value) -> Result<Value, String> {
        let mut state = self.lock();
        let ours = state.account.identity_keys();
        let entry = &content["ciphertext"][ours.curve25519.to_base64()];
        let kind = entry["type"]
            .as_u64()
            .ok_or("not addressed to this device")?;
        let body =
            base64_decode(entry["body"].as_str().unwrap_or_default()).map_err(|e| e.to_string())?;
        let message = OlmMessage::from_parts(usize::try_from(kind).unwrap_or(9), &body)
            .map_err(|e| e.to_string())?;
        let sender_key = content["sender_key"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let known = state.sessions.entry(sender_key.clone()).or_default();
        let mut plaintext = known.iter_mut().find_map(|s| s.decrypt(&message).ok());
        if plaintext.is_none()
            && let OlmMessage::PreKey(prekey) = &message
        {
            let their = Curve25519PublicKey::from_base64(&sender_key).map_err(|e| e.to_string())?;
            let created = state
                .account
                .create_inbound_session(SessionConfig::version_1(), their, prekey)
                .map_err(|e| e.to_string())?;
            state
                .sessions
                .entry(sender_key)
                .or_default()
                .push(created.session);
            plaintext = Some(created.plaintext);
        }
        let plaintext = plaintext.ok_or("no Olm session decrypts it")?;
        let payload: Value = serde_json::from_slice(&plaintext).map_err(|e| e.to_string())?;
        let addressed = payload["recipient"] == self.user.as_str()
            && payload["sender"] == sender
            && payload["recipient_keys"]["ed25519"] == ours.ed25519.to_base64().as_str();
        if !addressed {
            return Err("the payload names another sender or recipient".into());
        }
        Ok(payload)
    }

    /// Keeps the Megolm session an `m.room_key` payload shares.
    fn room_key(&self, payload: &Value, content: &Value) -> bool {
        let key = &payload["content"];
        if payload["type"] != "m.room_key" || key["algorithm"] != MEGOLM {
            return false;
        }
        let (Some(room), Some(id), Some(session_key)) = (
            key["room_id"].as_str(),
            key["session_id"].as_str(),
            key["session_key"].as_str(),
        ) else {
            return false;
        };
        let Ok(session_key) = SessionKey::from_base64(session_key) else {
            return false;
        };
        let session =
            InboundGroupSession::new(&session_key, vodozemac::megolm::SessionConfig::version_1());
        if session.session_id() != id {
            return false;
        }
        let entry = store::Inbound {
            room_id: room.to_owned(),
            sender_key: content["sender_key"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            pickle: String::new(),
        };
        self.lock()
            .inbound
            .entry(format!("{room} {id}"))
            .or_insert((entry, session));
        true
    }

    /// `event` (an `m.room.encrypted` event of `room`) decrypted: its type
    /// and content replaced by the plaintext's.
    ///
    /// # Errors
    ///
    /// Why it cannot be decrypted (no room key for it yet, say).
    pub(crate) fn decrypt(&self, room: &str, event: &Value) -> Result<Value, String> {
        let content = &event["content"];
        if content["algorithm"] != MEGOLM {
            return Err(format!("unsupported algorithm {}", content["algorithm"]));
        }
        let id = content["session_id"].as_str().unwrap_or_default();
        let message =
            MegolmMessage::from_base64(content["ciphertext"].as_str().unwrap_or_default())
                .map_err(|e| e.to_string())?;
        let mut state = self.lock();
        let (entry, session) = state
            .inbound
            .get_mut(&format!("{room} {id}"))
            .ok_or("no room key for its session yet")?;
        if content["sender_key"]
            .as_str()
            .is_some_and(|k| k != entry.sender_key)
        {
            return Err("its sender key is not the one that shared the session".into());
        }
        let decrypted = session.decrypt(&message).map_err(|e| e.to_string())?;
        let plaintext: Value =
            serde_json::from_slice(&decrypted.plaintext).map_err(|e| e.to_string())?;
        if plaintext["room_id"] != room {
            return Err("it was encrypted for another room".into());
        }
        let mut clear = event.clone();
        clear["type"] = plaintext["type"].clone();
        clear["content"] = plaintext["content"].clone();
        Ok(clear)
    }
}

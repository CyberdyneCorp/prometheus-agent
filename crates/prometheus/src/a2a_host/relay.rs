//! The sending side of the desktop relay (spec: agent-interop, Desktop
//! relay for cross-connection bot messages): a bot message for an agent on
//! another desktop connection becomes a signed envelope in the sending
//! profile's outbox, which a courier drains and carries; the sender reads
//! its outcome here. The signature (HMAC-SHA256 under a key only this
//! profile's store holds) makes a reply settle only the envelope a courier
//! actually carried.

use jiff::Timestamp;
use pmt_config::schema::BotModeConfig;
use pmt_core::error::Error;
use pmt_store::Store;
use pmt_store::bot_relay::{Envelope, RelayState, RelayTiming};

/// How long a courier has to deliver a drained envelope (the courier's
/// delivery deadline: a turn bound of 1500 s plus a minute of transport).
pub const DELIVERY_DEADLINE_SECS: u64 = 1560;

/// The store preference holding the profile's relay signing key.
const KEY_PREF: &str = "bot_relay.signing_key";

/// The relay's timing under `config`.
#[must_use]
pub fn timing(config: &BotModeConfig) -> RelayTiming {
    let ms = |secs: u64| i64::try_from(secs.saturating_mul(1000)).unwrap_or(i64::MAX);
    RelayTiming {
        deadline_ms: ms(DELIVERY_DEADLINE_SECS),
        ttl_ms: ms(config.envelope_ttl_secs),
    }
}

/// The profile's signing key, made on first use.
fn key(store: &Store) -> Result<String, Error> {
    if let Some(key) = store.pref(KEY_PREF)? {
        return Ok(key);
    }
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| {
        Error::new(
            &pmt_core::error::codes::HOME_IO,
            format!("no randomness for the relay key: {e}"),
        )
    })?;
    let key = pmt_a2a::sign::hex(&bytes);
    store.set_pref(KEY_PREF, &key)?;
    Ok(key)
}

/// The signature of an envelope's fields.
fn signature(
    key: &str,
    id: &str,
    sender: &str,
    target: &str,
    message: &str,
    created: Timestamp,
) -> String {
    let text = format!("{id}\n{sender}\n{target}\n{created}\n{message}");
    pmt_a2a::sign::hex(&pmt_a2a::sign::hmac_sha256(key.as_bytes(), text.as_bytes()))
}

/// Queues a bot message from `sender` to `target` (`<handle>@<connection>`)
/// in `store`'s outbox; the envelope.
///
/// # Errors
///
/// Store errors.
pub fn enqueue(
    store: &Store,
    sender: &str,
    target: &str,
    message: &str,
) -> Result<Envelope, Error> {
    let key = key(store)?;
    let delivery_id = format!("dlv_{}", ulid::Ulid::generate().to_string().to_lowercase());
    // Whole milliseconds, as the outbox stores them.
    let created = Timestamp::from_millisecond(Timestamp::now().as_millisecond())
        .unwrap_or_else(|_| Timestamp::now());
    let envelope = Envelope {
        signature: signature(&key, &delivery_id, sender, target, message, created),
        delivery_id,
        sender: sender.to_owned(),
        target: target.to_owned(),
        message: message.to_owned(),
        created_at: created,
    };
    store.relay_enqueue(&envelope)?;
    Ok(envelope)
}

/// Where delivery `id` is at `now`: an envelope past twice the delivery
/// deadline since its first offer is failed `delivery_timeout` first.
///
/// # Errors
///
/// Store errors.
pub fn outcome(
    store: &Store,
    id: &str,
    now: Timestamp,
    config: &BotModeConfig,
) -> Result<Option<RelayState>, Error> {
    store.relay_time_out(now, timing(config))?;
    store.relay_state(id)
}

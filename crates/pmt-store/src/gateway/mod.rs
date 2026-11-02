//! The messaging gateway's per-profile state (spec: messaging-gateway):
//! [`routes`] maps conversation keys to sessions and keeps each routed
//! session's gateway state (reset reason, suspension, crash-recovery
//! bookkeeping), [`pairing`] holds pairing requests and approved senders,
//! and [`outbox`] every outbound message until it is delivered.

pub mod outbox;
pub mod pairing;
pub mod routes;

use jiff::Timestamp;

pub(crate) fn timestamp(ms: i64) -> Timestamp {
    Timestamp::from_millisecond(ms).unwrap_or(Timestamp::UNIX_EPOCH)
}

pub(crate) fn millis(at: Timestamp) -> i64 {
    at.as_millisecond()
}

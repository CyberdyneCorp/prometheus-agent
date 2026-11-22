//! What the pipeline keeps in profile state (shared by the gateway's
//! instance and `prometheus plugins run`, so always read fresh):
//! subscriptions (`subs`), meetings (`meeting:<id>`), the recent
//! notifications with how each fared (`notifications`) and the replays
//! asked for (`replays`).

use adapter_kit::host;
use serde_json::{Value, json};

/// Notifications remembered for `status`.
const NOTIFICATIONS_KEPT: usize = 200;

/// The subscriptions recorded.
pub(crate) fn subscriptions() -> Vec<Value> {
    host::load("subs")
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
}

pub(crate) fn store_subscriptions(subs: &[Value]) {
    host::store("subs", &Value::Array(subs.to_vec()));
}

/// Records (or replaces) subscription `sub` by its `id`.
pub(crate) fn upsert_subscription(sub: Value) {
    let mut subs = subscriptions();
    subs.retain(|s| s["id"] != sub["id"]);
    subs.push(sub);
    store_subscriptions(&subs);
}

/// A meeting's record (`Null` when unknown).
pub(crate) fn meeting(id: &str) -> Value {
    host::load(&format!("meeting:{id}")).unwrap_or(Value::Null)
}

pub(crate) fn store_meeting(id: &str, record: &Value) {
    host::store(&format!("meeting:{id}"), record);
}

/// Every meeting record, with its id.
pub(crate) fn meetings() -> Vec<(String, Value)> {
    host::keys()
        .into_iter()
        .filter_map(|k| k.strip_prefix("meeting:").map(str::to_owned))
        .map(|id| {
            let record = meeting(&id);
            (id, record)
        })
        .collect()
}

/// Records how notification `id` fared.
pub(crate) fn note(id: &str, meeting: &str, artifact: &str, status: &str) {
    let mut log = host::load("notifications")
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();
    log.push(json!({"id": id, "meeting": meeting, "artifact": artifact, "status": status}));
    let start = log.len().saturating_sub(NOTIFICATIONS_KEPT);
    host::store("notifications", &Value::Array(log[start..].to_vec()));
}

/// Marks the queued notifications of `meeting` with `status`.
pub(crate) fn settle_notes(meeting: &str, status: &str) {
    let Some(mut log) = host::load("notifications").and_then(|v| v.as_array().cloned()) else {
        return;
    };
    for entry in &mut log {
        if entry["meeting"] == meeting && entry["status"] == "queued" {
            entry["status"] = json!(status);
        }
    }
    host::store("notifications", &Value::Array(log));
}

/// The recent notifications.
pub(crate) fn notifications() -> Vec<Value> {
    host::load("notifications")
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
}

/// Asks the running gateway to summarize `meeting` again.
pub(crate) fn request_replay(meeting: &str) {
    let mut replays = host::load("replays")
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();
    replays.push(json!(meeting));
    host::store("replays", &Value::Array(replays));
}

/// The replays asked for since the last call (taken).
pub(crate) fn take_replays() -> Vec<String> {
    let Some(replays) = host::load("replays").and_then(|v| v.as_array().cloned()) else {
        return Vec::new();
    };
    if replays.is_empty() {
        return Vec::new();
    }
    host::store("replays", &json!([]));
    replays
        .iter()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect()
}

//! What the pipeline keeps in profile state (shared by the gateway's
//! instance and `prometheus plugins run`, so always read fresh):
//! subscriptions (`subs`), meetings (`meeting:<id>`, each with how every
//! summary key of it fared under `artifacts`), the recent notifications
//! with how each fared (`notifications`) and the replays asked for
//! (`replays`).

use adapter_kit::host;
use serde_json::{Value, json};

/// Notifications remembered for `status`.
const NOTIFICATIONS_KEPT: usize = 200;
/// How long a queued summary holds its key: a turn that never delivered
/// (it failed, or the gateway gave its delivery up) stops blocking a later
/// notification for the same transcript after this.
const QUEUED_TTL_SECS: u64 = 3600;

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

fn store_meeting(id: &str, record: &Value) {
    host::store(&format!("meeting:{id}"), record);
}

/// Whether an `artifacts` entry holds its key: summarized, or queued
/// less than the TTL ago (or at a time unknown).
fn holds(entry: &Value, now: Option<u64>) -> bool {
    match entry["status"].as_str() {
        Some("summarized") => true,
        Some("queued") => match (entry["at"].as_u64(), now) {
            (Some(at), Some(now)) => now < at.saturating_add(QUEUED_TTL_SECS),
            _ => true,
        },
        _ => false,
    }
}

/// Whether summary key `key` of meeting `id` is already summarized or
/// on its way (a notification for it is then a `duplicate`).
pub(crate) fn held(id: &str, key: &str, now: Option<u64>) -> bool {
    holds(&meeting(id)["artifacts"][key], now)
}

/// Records `status` (`queued`, `summarized` or `failed`) for summary key
/// `key` of meeting `id`, at `now`; `path` is the meeting's Graph path.
pub(crate) fn mark(id: &str, key: &str, status: &str, now: Option<u64>, path: Option<&str>) {
    let mut record = meeting(id);
    if !record.is_object() {
        record = json!({"delivered": 0});
    }
    if !record["artifacts"].is_object() {
        record["artifacts"] = json!({});
    }
    record["artifacts"][key] = json!({"status": status, "at": now});
    record["status"] = json!(status);
    record["artifact"] = json!(key);
    if let Some(path) = path {
        record["path"] = json!(path);
    }
    if status == "summarized" {
        record["delivered"] = json!(record["delivered"].as_u64().unwrap_or(0) + 1);
    }
    store_meeting(id, &record);
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

/// Marks the queued notifications of summary key `key` of `meeting` with
/// `status`.
pub(crate) fn settle_notes(meeting: &str, key: &str, status: &str) {
    let Some(mut log) = host::load("notifications").and_then(|v| v.as_array().cloned()) else {
        return;
    };
    for entry in &mut log {
        if entry["meeting"] == meeting && entry["artifact"] == key && entry["status"] == "queued" {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_queued_summary_stops_holding_its_key_after_the_ttl() {
        let queued = json!({"status": "queued", "at": 1000});
        assert!(holds(&queued, Some(1000 + QUEUED_TTL_SECS - 1)));
        assert!(!holds(&queued, Some(1000 + QUEUED_TTL_SECS)));
        assert!(holds(&queued, None), "an unknown time keeps holding");
        let summarized = json!({"status": "summarized", "at": 1000});
        assert!(holds(&summarized, Some(u64::MAX)));
        assert!(!holds(&json!({"status": "failed", "at": 1000}), Some(1000)));
        assert!(!holds(&Value::Null, Some(1000)));
    }
}

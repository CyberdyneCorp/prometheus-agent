//! Bulk maintenance (spec: web-dashboard, REST endpoint inventory, "Bulk
//! maintenance needs confirmation"): `POST maintenance/{action}` runs
//! `prune-sessions`, `vacuum`, `rebuild-fts` or `prune-snapshots` on the
//! addressed profile only when the body says `"confirm": true` (409 and
//! nothing touched otherwise). Observers never get here: the profile
//! router refuses every state-changing route for them with 403.

use hyper::body::Incoming;

use crate::Shared;
use crate::host::Change;
use crate::http::{Reply, error, from_error, json_reply, read_json};

/// The maintenance actions.
pub(crate) const ACTIONS: &[&str] = &["prune-sessions", "vacuum", "rebuild-fts", "prune-snapshots"];

pub(crate) async fn run(shared: &Shared, profile: &str, action: &str, body: Incoming) -> Reply {
    if !ACTIONS.contains(&action) {
        return error(
            404,
            "not_found",
            &format!(
                "unknown maintenance action {action} (expected {})",
                ACTIONS.join(", ")
            ),
        );
    }
    let body = match read_json(body).await {
        Ok(body) => body,
        Err(reply) => return reply,
    };
    if body["confirm"] != serde_json::Value::Bool(true) {
        return error(
            409,
            "confirm",
            &format!("{action} changes stored data; send {{\"confirm\": true}} to run it"),
        );
    }
    match shared
        .host
        .change(profile, Change::Maintenance, Some(action), body)
        .await
    {
        Ok(value) => json_reply(200, &value),
        Err(err) => from_error(&err),
    }
}

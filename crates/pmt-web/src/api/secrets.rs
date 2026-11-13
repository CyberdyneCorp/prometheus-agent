//! A profile's secrets (spec: web-dashboard, Secrets are write-only): names,
//! where each comes from and when the store last changed, set and delete.
//! No answer ever carries a value or part of one, and there is no reveal
//! endpoint.

use hyper::body::Incoming;
use pmt_config::secrets::{Source, store_path};
use pmt_core::secret::SecretString;
use serde_json::{Value, json};

use super::profile::Profile;
use crate::http::{Reply, error, from_error, json_reply, read_json};

/// When the profile's secrets file last changed (`None` for other stores).
fn file_modified(at: &Profile<'_>) -> Option<String> {
    let modified = std::fs::metadata(store_path(at.scope.dir()))
        .and_then(|m| m.modified())
        .ok()?;
    jiff::Timestamp::try_from(modified)
        .ok()
        .map(|t| t.to_string())
}

/// `GET secrets`.
pub(crate) fn list(at: &Profile<'_>) -> Reply {
    let rows = match at.scope.secrets().list() {
        Ok(rows) => rows,
        Err(err) => return from_error(&err),
    };
    let file_time = file_modified(at);
    let listed: Vec<Value> = rows
        .iter()
        .map(|row| {
            let modified = (row.source == Source::File)
                .then(|| file_time.clone())
                .flatten();
            json!({"name": row.name, "source": row.source, "variable": row.variable,
                   "shadowed": row.shadowed, "modified": modified})
        })
        .collect();
    json_reply(200, &json!({"secrets": listed}))
}

/// `GET secrets/{name}`: whether it is set and where from, never a value.
pub(crate) fn show(at: &Profile<'_>, name: &str) -> Reply {
    match at.scope.secrets().list() {
        Ok(rows) => match rows.iter().find(|row| row.name == name) {
            Some(row) => json_reply(
                200,
                &json!({"name": row.name, "set": true, "source": row.source, "shadowed": row.shadowed}),
            ),
            None => json_reply(200, &json!({"name": name, "set": false})),
        },
        Err(err) => from_error(&err),
    }
}

/// `PUT secrets/{name} {"value": "..."}`.
pub(crate) async fn put(at: &Profile<'_>, name: &str, body: Incoming) -> Reply {
    let body = match read_json(body).await {
        Ok(body) => body,
        Err(reply) => return reply,
    };
    let Some(value) = body["value"].as_str().filter(|v| !v.is_empty()) else {
        return error(400, "invalid_body", "expected {\"value\": \"<secret>\"}");
    };
    let stored = pmt_config::managed::check_secret_writable(name).and_then(|()| {
        at.scope
            .secrets()
            .store()
            .set(name, &SecretString::new(value))
    });
    match stored {
        Ok(()) => json_reply(200, &json!({"name": name, "saved": true})),
        Err(err) => from_error(&err),
    }
}

/// `DELETE secrets/{name}`.
pub(crate) fn delete(at: &Profile<'_>, name: &str) -> Reply {
    let removed = pmt_config::managed::check_secret_writable(name)
        .and_then(|()| at.scope.secrets().store().remove(name));
    match removed {
        Ok(true) => json_reply(200, &json!({"name": name, "deleted": true})),
        Ok(false) => error(404, "not_found", &format!("no stored secret {name}")),
        Err(err) => from_error(&err),
    }
}

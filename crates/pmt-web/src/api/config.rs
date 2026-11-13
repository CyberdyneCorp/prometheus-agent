//! A profile's configuration (spec: web-dashboard, Config editing is
//! schema-driven): the effective values, the JSON Schema of the running
//! build (which the `/config` page renders its forms from), the defaults,
//! and edits saved as one atomic validated write of `config.toml` (the
//! same writer as `prometheus config set`). Each saved key reports when it
//! applies (`live`, `next_session` or `restart`).

use hyper::body::Incoming;
use pmt_config::catalog;
use serde_json::{Value, json};

use super::profile::Profile;
use crate::http::{Reply, error, json_reply, read_json};

/// `GET config`: the effective configuration.
pub(crate) fn get(at: &Profile<'_>) -> Reply {
    match serde_json::to_value(at.scope.config()) {
        Ok(values) => json_reply(200, &json!({"profile": at.name, "values": values})),
        Err(e) => error(500, "internal", &e.to_string()),
    }
}

/// `GET config/schema`.
pub(crate) fn schema() -> Reply {
    json_reply(200, catalog::schema())
}

/// `GET config/defaults`.
pub(crate) fn defaults() -> Reply {
    json_reply(
        200,
        &serde_json::to_value(pmt_config::Config::default()).unwrap_or_default(),
    )
}

/// The raw text `config set` takes for a JSON value.
fn raw(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// `PATCH config {"set": {"<key>": <value>, ...}}`: all keys or none.
pub(crate) async fn patch(at: &Profile<'_>, body: Incoming) -> Reply {
    let body = match read_json(body).await {
        Ok(body) => body,
        Err(reply) => return reply,
    };
    let Some(set) = body["set"].as_object().filter(|s| !s.is_empty()) else {
        return error(
            400,
            "invalid_body",
            "expected {\"set\": {\"<key>\": <value>}}",
        );
    };
    let texts: Vec<(String, String)> = set.iter().map(|(k, v)| (k.clone(), raw(v))).collect();
    let pairs: Vec<(&str, &str)> = texts
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    if let Err(err) = at.scope.profile().config_file().set_all(&pairs) {
        return json_reply(
            400,
            &json!({"error": err.code().code, "message": err.message(), "hint": err.hint()}),
        );
    }
    let applies: serde_json::Map<String, Value> = set
        .keys()
        .map(|key| {
            let apply = catalog::lookup(key).map_or("live", |info| info.apply.as_str());
            (key.clone(), apply.into())
        })
        .collect();
    json_reply(200, &json!({"saved": true, "applies": applies}))
}

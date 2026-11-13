//! Channel setup forms (spec: web-dashboard, Channel setup forms): every
//! adapter available to the profile with its configuration and messaging
//! state, and its configure form rendered from the adapter's options
//! schema. Secret fields are write-only (a blank value keeps the stored
//! one). A save is one validated config patch plus the secret writes:
//! the secrets are checked first, the patch is validated as a whole
//! (a missing required field is refused naming it, and nothing is
//! written), and the secrets are written only after it was saved.

use hyper::body::Incoming;
use pmt_config::platforms;
use pmt_config::secrets::surface::BUILTIN;
use pmt_core::secret::SecretString;
use serde_json::{Map, Value, json};

use super::profile::Profile;
use crate::Shared;
use crate::http::{Reply, error, from_error, json_reply, read_json};

/// The secrets of built-in adapter `platform` (`MATRIX_ACCESS_TOKEN`...).
fn secret_names(platform: &str) -> Vec<&'static str> {
    let prefix = format!("{}_", platform.to_ascii_uppercase());
    BUILTIN
        .iter()
        .copied()
        .filter(|name| name.starts_with(&prefix))
        .collect()
}

fn available() -> Vec<String> {
    platforms::FIRST_PARTY
        .iter()
        .map(|p| p.name.to_owned())
        .chain(platforms::plugin_platforms())
        .collect()
}

/// `GET adapters`: every available adapter with its form.
pub(crate) fn list(shared: &Shared, at: &Profile<'_>) -> Reply {
    let running = shared.server.status().adapters;
    let config = at.scope.config();
    let secrets = at.scope.secrets();
    let adapters: Vec<Value> = available()
        .iter()
        .map(|name| {
            let configured = config.gateway.platforms.get(name);
            let status = running
                .iter()
                .find(|a| a.profile == at.name && a.platform == *name);
            let state = status.map_or_else(
                || {
                    if configured.is_some_and(|c| c.enabled) {
                        "stopped"
                    } else {
                        "disabled"
                    }
                    .to_owned()
                },
                |s| s.state.clone(),
            );
            let secret_rows: Vec<Value> = secret_names(name)
                .into_iter()
                .map(|s| json!({"name": s, "set": secrets.resolve(s).is_ok()}))
                .collect();
            json!({
                "platform": name,
                "display": platforms::first_party(name).map_or(name.as_str(), |p| p.display),
                "enabled": configured.is_some_and(|c| c.enabled),
                "state": state,
                "paused": status.is_some_and(|s| s.paused),
                "message": status.and_then(|s| s.message.clone()),
                "options": configured.map(|c| serde_json::to_value(&c.options).unwrap_or_default()),
                "schema": platforms::options_schema(name),
                "pairing": platforms::first_party(name).and_then(|p| p.pairing).map(platforms::PairingFlow::as_str),
                "secrets": secret_rows,
            })
        })
        .collect();
    json_reply(200, &json!({"adapters": adapters}))
}

/// The raw text `config set` takes for a JSON value.
fn raw(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// `PUT adapters/{name} {"enabled": bool, "options": {...}, "secrets": {...}}`.
pub(crate) async fn save(at: &Profile<'_>, name: &str, body: Incoming) -> Reply {
    if !available().iter().any(|p| p == name) {
        return error(404, "not_found", &format!("no adapter {name}"));
    }
    let body = match read_json(body).await {
        Ok(body) => body,
        Err(reply) => return reply,
    };
    let allowed = secret_names(name);
    let empty = Map::new();
    let secrets: Vec<(&str, &str)> = body["secrets"]
        .as_object()
        .unwrap_or(&empty)
        .iter()
        .filter_map(|(k, v)| Some((k.as_str(), v.as_str()?)))
        .filter(|(_, v)| !v.is_empty())
        .collect();
    if let Some((bad, _)) = secrets.iter().find(|(k, _)| !allowed.contains(k)) {
        return error(
            400,
            "invalid_secret",
            &format!("{bad} is not a secret of the {name} adapter"),
        );
    }
    if let Some(err) = secrets
        .iter()
        .find_map(|(k, _)| pmt_config::managed::check_secret_writable(k).err())
    {
        return from_error(&err);
    }
    let mut texts: Vec<(String, String)> = body["options"]
        .as_object()
        .unwrap_or(&empty)
        .iter()
        .filter(|(_, v)| !v.is_null() && v.as_str() != Some(""))
        .map(|(k, v)| (format!("gateway.platforms.{name}.options.{k}"), raw(v)))
        .collect();
    if let Some(enabled) = body["enabled"].as_bool() {
        texts.push((
            format!("gateway.platforms.{name}.enabled"),
            enabled.to_string(),
        ));
    }
    if !texts.is_empty() {
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
    }
    let store = at.scope.secrets().store();
    for (key, value) in &secrets {
        if let Err(err) = store.set(key, &SecretString::new(*value)) {
            return from_error(&err);
        }
    }
    json_reply(
        200,
        &json!({"platform": name, "saved": true, "applies": "restart"}),
    )
}

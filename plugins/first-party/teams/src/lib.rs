//! The Microsoft Teams adapter (spec: platform-adapters, Microsoft Teams
//! adapter): Bot Framework activities arrive on the shared listener (the
//! bot's messaging endpoint, bodies capped at 1 MiB), each admitted only
//! once its Bot Framework JWT verifies ([`auth`]); a request without one
//! is answered 401 and admits nothing. [`activity`] turns activities into
//! messages, [`outbound`] answers through the Bot Connector. Options
//! `client_id` (the bot's app id) and `tenant_id`, and the secret
//! `TEAMS_CLIENT_SECRET`, are required.

mod activity;
mod auth;
mod outbound;

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use adapter_kit::message::{self, Inbound, Outgoing};
use adapter_kit::{Guest, HttpRequest, HttpResponse, clock, host, respond};
use serde_json::{Value, json};

use auth::Auth;

/// A card waiting for its answer: what each number stands for.
#[derive(Debug, Clone)]
struct Choices {
    chat: String,
    data: Vec<String>,
}

/// The started adapter.
struct Teams {
    client_id: String,
    require_mention: bool,
    default_service: String,
    auth: Auth,
    /// Each conversation's service URL.
    services: HashMap<String, String>,
    inbox: Vec<Inbound>,
    /// Activity ids the bot sent (newest last).
    sent: Vec<String>,
    /// Activity ids of the cards it sent.
    cards: Vec<String>,
    choices: Vec<Choices>,
}

static TEAMS: Mutex<Option<Teams>> = Mutex::new(None);

fn lock() -> Result<MutexGuard<'static, Option<Teams>>, String> {
    TEAMS.lock().map_err(|e| e.to_string())
}

fn with<R>(f: impl FnOnce(&mut Teams) -> Result<R, String>) -> Result<R, String> {
    let mut guard = lock()?;
    f(guard.as_mut().ok_or("not started")?)
}

fn required<'a>(options: &'a Value, key: &str) -> Result<&'a str, String> {
    options[key]
        .as_str()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| format!("options.{key} is required"))
}

fn start(options: &Value) -> Result<Teams, String> {
    let client_id = required(options, "client_id")?.to_owned();
    let tenant = required(options, "tenant_id")?;
    if host::secret("TEAMS_CLIENT_SECRET").is_none() {
        return Err("secret TEAMS_CLIENT_SECRET is not set".to_owned());
    }
    Ok(Teams {
        auth: Auth::new(options, &client_id, tenant),
        client_id,
        require_mention: options["require_mention"].as_bool().unwrap_or(true),
        default_service: options["service_url"]
            .as_str()
            .unwrap_or("https://smba.trafficmanager.net/teams")
            .trim_end_matches('/')
            .to_owned(),
        services: HashMap::new(),
        inbox: Vec::new(),
        sent: Vec::new(),
        cards: Vec::new(),
        choices: Vec::new(),
    })
}

/// One request to the messaging endpoint.
fn receive(teams: &mut Teams, request: &HttpRequest) -> HttpResponse {
    let Ok(activity) = serde_json::from_slice::<Value>(&request.body) else {
        return respond(400, "text/plain", "not an activity");
    };
    let authorization = adapter_kit::header(request, "authorization").unwrap_or_default();
    let service = activity["serviceUrl"].as_str().unwrap_or_default();
    if let Err(why) = teams.auth.verify(authorization, service) {
        host::warn(&format!("refused a Teams activity: {why}"));
        return respond(401, "text/plain", "unauthorized");
    }
    match activity["type"].as_str() {
        Some("message") => teams.activity(&activity),
        Some("invoke") if activity["name"] == "adaptiveCard/action" => {
            teams.activity(&activity);
            let answer = json!({
                "statusCode": 200,
                "type": "application/vnd.microsoft.activity.message",
                "value": "Received",
            });
            return respond(200, "application/json", answer.to_string());
        }
        _ => {
            if let (Some(chat), false) =
                (activity["conversation"]["id"].as_str(), service.is_empty())
            {
                teams.remember_service(chat, service);
            }
        }
    }
    respond(200, "application/json", "{}")
}

struct Adapter;

impl Guest for Adapter {
    fn descriptor() -> String {
        json!({
            "platform": "teams",
            "max_message_length": 28000,
            "length_unit": "chars",
            "splits_long_messages": false,
            "supports_edit": true,
            "supports_delete": true,
            "supports_typing": true,
            "supports_reactions": false,
            "supports_threads": false,
            "interactive": "native",
            "media_in": ["image", "document"],
            "media_out": [],
            "delivery_receipts": false,
            "read_receipts": false,
            "streaming_tts": false,
            "markup": "markdown",
            "binds_http": true,
            "max_body_bytes": 1_048_576,
        })
        .to_string()
    }

    fn start(options: String) -> Result<(), String> {
        let options: Value = serde_json::from_str(&options).map_err(|e| e.to_string())?;
        *lock()? = Some(start(&options)?);
        Ok(())
    }

    fn stop() {
        if let Ok(mut teams) = lock() {
            *teams = None;
        }
    }

    fn poll() -> Result<String, String> {
        clock::tick();
        with(|t| Ok(message::batch(&std::mem::take(&mut t.inbox))))
    }

    fn send(message: String) -> Result<String, String> {
        let out = Outgoing::parse(&message)?;
        with(|t| match out.op.as_str() {
            "" => t.send(&out),
            "edit" => t.edit(&out).map(|()| String::new()),
            "delete" => t.delete(&out).map(|()| String::new()),
            "typing" => t.typing(&out.chat).map(|()| String::new()),
            _ => Err("unsupported".to_owned()),
        })
    }

    fn chat_info(chat: String) -> Result<String, String> {
        // Personal conversation ids start `a:`; group chats `19:...@thread`.
        let kind = if chat.starts_with("19:") {
            if chat.contains("@thread.tacv2") || chat.contains("@thread.skype") {
                "channel"
            } else {
                "group"
            }
        } else {
            "dm"
        };
        Ok(message::chat_info(&chat, kind))
    }

    fn handle_http(request: HttpRequest) -> HttpResponse {
        if request.method != "POST" {
            return respond(405, "text/plain", "");
        }
        let Ok(mut guard) = lock() else {
            return respond(503, "text/plain", "busy");
        };
        match guard.as_mut() {
            Some(teams) => receive(teams, &request),
            None => respond(503, "text/plain", "not started"),
        }
    }

    fn authorize_sender(_inbound: String) -> String {
        // The gateway's allowlists (AAD object ids) decide.
        "unknown".to_owned()
    }
}

adapter_kit::bindings::export_adapter!(Adapter);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mentions_are_stripped() {
        assert_eq!(
            activity::strip_mentions("<at>Prometheus</at> deploy  it"),
            "deploy it"
        );
        assert_eq!(activity::strip_mentions("no mention"), "no mention");
    }
}

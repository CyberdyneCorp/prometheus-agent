//! A2A push notifications (spec: agent-interop, A2A push notifications).
//! A configured URL receives every status change of its task, signed with
//! HMAC-SHA256 under the profile's `a2a.server.push_secret` (else the
//! authenticating peer's token). URLs go through the outbound egress
//! policy when registered and again when sent; a failed push is retried 3
//! times with exponential backoff from 2 seconds, then recorded on the
//! task, which `GetTask` shows.

use std::sync::Arc;
use std::time::Duration;

use pmt_a2a::sign::{self, SIGNATURE_HEADER, TIMESTAMP_HEADER};
use pmt_a2a::wire::Dialect;
use serde_json::{Value, json};

use super::A2a;
use super::tasks::{PushConfig, Task};
use crate::host::Push;

/// Retries after the first failed attempt.
const RETRIES: u32 = 3;
/// The first retry's delay; each later one doubles it.
const FIRST_BACKOFF: Duration = Duration::from_secs(2);

/// The configuration a create request carries: v1.0 puts it at the top
/// level (`TaskPushNotificationConfig`), v0.3 under
/// `pushNotificationConfig`.
pub(super) fn parse_config(params: &Value) -> Option<(Option<String>, PushConfig)> {
    let config = if params["pushNotificationConfig"].is_object() {
        &params["pushNotificationConfig"]
    } else if params["config"].is_object() {
        &params["config"]
    } else {
        params
    };
    let url = config["url"].as_str().filter(|u| !u.is_empty())?.to_owned();
    let id = config["id"].as_str().filter(|s| !s.is_empty()).map_or_else(
        || ulid::Ulid::generate().to_string().to_lowercase(),
        str::to_owned,
    );
    let task = params["taskId"].as_str().map(str::to_owned);
    let token = config["token"].as_str().map(str::to_owned);
    Some((task, PushConfig { id, url, token }))
}

/// A configuration as answered in `dialect`.
pub(super) fn config_json(task_id: &str, config: &PushConfig, dialect: Dialect) -> Value {
    let inner = json!({"id": config.id, "url": config.url, "token": config.token});
    match dialect {
        Dialect::V1 => json!({
            "taskId": task_id,
            "id": config.id,
            "url": config.url,
            "token": config.token,
        }),
        Dialect::V03 => json!({"taskId": task_id, "pushNotificationConfig": inner}),
    }
}

/// Sends `task`'s status to each of its push URLs, each on a task of its
/// own (a slow receiver never holds the turn).
pub(super) fn notify(a2a: &Arc<A2a>, task: &Task) {
    let Some(secret) = task.push_secret.clone() else {
        return;
    };
    for config in &task.push {
        let a2a = Arc::clone(a2a);
        let task = task.clone();
        let config = config.clone();
        let secret = secret.clone();
        pmt_core::spawn::spawn(async move {
            if let Err(error) = deliver(&a2a, &task, &config, &secret).await {
                tracing::warn!(task = task.id, url = config.url, "a2a push failed: {error}");
                a2a.tasks.update(&task.id, |t| t.push_error = Some(error));
            }
        });
    }
}

async fn deliver(a2a: &A2a, task: &Task, config: &PushConfig, secret: &str) -> Result<(), String> {
    let body = task.status_event(task.dialect).to_string();
    let mut delay = FIRST_BACKOFF;
    let mut last = String::new();
    for attempt in 0..=RETRIES {
        if attempt > 0 {
            tokio::time::sleep(delay).await;
            delay *= 2;
        }
        let timestamp = jiff::Timestamp::now().as_second();
        let mut headers = vec![
            ("content-type".to_owned(), "application/json".to_owned()),
            (TIMESTAMP_HEADER.to_owned(), timestamp.to_string()),
            (
                SIGNATURE_HEADER.to_owned(),
                sign::signature(secret.as_bytes(), timestamp, body.as_bytes()),
            ),
        ];
        if let Some(token) = &config.token {
            headers.push(("X-A2A-Notification-Token".to_owned(), token.clone()));
        }
        let push = Push {
            profile: &task.profile,
            url: &config.url,
            headers: &headers,
            body: body.as_bytes(),
        };
        match a2a.api.host.api_push(push).await {
            Ok(status) if (200..300).contains(&status) => return Ok(()),
            Ok(status) => last = format!("{} answered HTTP {status}", config.url),
            Err(err) => err.message().clone_into(&mut last),
        }
    }
    Err(format!("{last} (after {RETRIES} retries)"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configurations_read_in_either_dialect() {
        let v1 = json!({"taskId": "t", "url": "https://h/hook", "token": "x"});
        let (task, config) = parse_config(&v1).unwrap();
        assert_eq!(task.as_deref(), Some("t"));
        assert_eq!(config.url, "https://h/hook");
        let v03 =
            json!({"taskId": "t", "pushNotificationConfig": {"id": "p1", "url": "https://h/2"}});
        let (_, config) = parse_config(&v03).unwrap();
        assert_eq!(
            (config.id.as_str(), config.url.as_str()),
            ("p1", "https://h/2")
        );
        assert!(parse_config(&json!({"taskId": "t"})).is_none());
    }
}

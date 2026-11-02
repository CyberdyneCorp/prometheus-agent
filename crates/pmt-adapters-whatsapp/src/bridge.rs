//! The `WhatsApp` Web bridge: a loopback HTTP service that holds the linked
//! device session and speaks the multi-device protocol. The adapter polls
//! `GET /messages` (the bridge queues inbound messages until they are
//! fetched, so a reconnect loses nothing), sends with `POST /send`,
//! `/edit`, `/send-media` and `/typing`, reads `GET /chat/<id>` and
//! watches `GET /health`. Without `options.bridge_url` the adapter starts
//! the managed bridge runtime (`<home>/runtimes/whatsapp-bridge/`, run by
//! the managed Node runtime) on a free loopback port.

use std::path::{Path, PathBuf};
use std::time::Duration;

use pmt_gateway::messaging::{AdapterError, SendError};
use serde_json::Value;

use crate::PLATFORM;

/// A bridge client.
#[derive(Clone)]
pub(crate) struct Bridge {
    pub(crate) client: reqwest::Client,
    /// `http://127.0.0.1:<port>`, no trailing slash.
    pub(crate) url: String,
}

impl std::fmt::Debug for Bridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bridge")
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

const CALL_TIMEOUT: Duration = Duration::from_mins(1);

fn transport(err: reqwest::Error) -> SendError {
    if err.is_timeout() {
        SendError::Ambiguous(err.without_url().to_string())
    } else {
        SendError::Network(err.without_url().to_string())
    }
}

impl Bridge {
    /// `GET` a bridge path.
    pub(crate) async fn get(&self, path: &str) -> Result<Value, SendError> {
        let sent = self
            .client
            .get(format!("{}{path}", self.url))
            .timeout(CALL_TIMEOUT)
            .send()
            .await;
        answer(sent).await
    }

    /// `POST` JSON to a bridge path.
    pub(crate) async fn post(&self, path: &str, body: &Value) -> Result<Value, SendError> {
        let sent = self
            .client
            .post(format!("{}{path}", self.url))
            .header("content-type", "application/json")
            .body(body.to_string())
            .timeout(CALL_TIMEOUT)
            .send()
            .await;
        answer(sent).await
    }
}

async fn answer(sent: Result<reqwest::Response, reqwest::Error>) -> Result<Value, SendError> {
    let response = sent.map_err(transport)?;
    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse().ok());
    let text = response.text().await.map_err(transport)?;
    let body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if (200..300).contains(&status) {
        return Ok(body);
    }
    let error = body["error"].as_str().unwrap_or("request failed");
    Err(refusal(status, error, retry_after))
}

/// The send error of a bridge refusal.
pub(crate) fn refusal(status: u16, error: &str, retry_after: Option<u64>) -> SendError {
    match status {
        429 => SendError::RateLimited {
            retry_after: retry_after.map(Duration::from_secs),
        },
        // 503: the bridge lost WhatsApp for now.
        500.. => SendError::Server(format!("{status} {error}")),
        _ => SendError::Permanent(format!("{status} {error}")),
    }
}

/// What the bridge's health says about the linked device.
pub(crate) fn health_error(health: &Value, profile: &str) -> Option<AdapterError> {
    match health["status"].as_str().unwrap_or_default() {
        "connected" => None,
        "logged_out" => Some(AdapterError {
            retryable: false,
            code: "whatsapp_logged_out".to_owned(),
            message: format!(
                "WhatsApp logged this device out; link it again with `prometheus -p {profile} gateway whatsapp pair`"
            ),
        }),
        other => Some(AdapterError {
            retryable: true,
            code: "whatsapp_disconnected".to_owned(),
            message: format!("the WhatsApp bridge is not connected ({other})"),
        }),
    }
}

/// Where the managed bridge runtime lives.
pub(crate) fn script(home: &Path) -> PathBuf {
    home.join("runtimes")
        .join("whatsapp-bridge")
        .join("bridge.js")
}

/// Starts the managed bridge for a profile: its session under
/// `<profile>/platforms/whatsapp/session/`, on a free loopback port. The
/// child dies with the returned handle.
pub(crate) fn launch(
    home: &Path,
    profile_dir: &Path,
    profile: &str,
) -> Result<(tokio::process::Child, String), AdapterError> {
    let missing = || AdapterError {
        retryable: false,
        code: "whatsapp_bridge_missing".to_owned(),
        message: format!(
            "the WhatsApp bridge runtime is not installed; run `prometheus -p {profile} gateway whatsapp pair`"
        ),
    };
    let script = script(home);
    if !script.is_file() {
        return Err(missing());
    }
    let node = pmt_update::runtimes::Store::open(home)
        .installed("node")
        .ok_or_else(missing)?
        .exe();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map_err(|e| AdapterError {
            retryable: true,
            code: "whatsapp_bridge_failed".to_owned(),
            message: format!("no loopback port for the bridge: {e}"),
        })?
        .port();
    let session = profile_dir.join("platforms").join(PLATFORM).join("session");
    let child = tokio::process::Command::new(node)
        .arg(&script)
        .arg("--port")
        .arg(port.to_string())
        .arg("--session")
        .arg(&session)
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .spawn()
        .map_err(|e| AdapterError {
            retryable: true,
            code: "whatsapp_bridge_failed".to_owned(),
            message: format!("cannot start the WhatsApp bridge: {e}"),
        })?;
    Ok((child, format!("http://127.0.0.1:{port}")))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn refusals_and_health_map_onto_errors() {
        assert_eq!(
            refusal(429, "slow down", Some(7)),
            SendError::RateLimited {
                retry_after: Some(Duration::from_secs(7))
            }
        );
        assert!(matches!(refusal(503, "x", None), SendError::Server(_)));
        assert!(matches!(refusal(400, "x", None), SendError::Permanent(_)));
        assert_eq!(health_error(&json!({"status": "connected"}), "work"), None);
        let out = health_error(&json!({"status": "logged_out"}), "work").unwrap();
        assert_eq!(out.code, "whatsapp_logged_out");
        assert!(!out.retryable);
        assert!(
            out.message
                .contains("prometheus -p work gateway whatsapp pair")
        );
        assert!(
            health_error(&json!({"status": "connecting"}), "work")
                .unwrap()
                .retryable
        );
    }
}

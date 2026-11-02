//! The signal-cli HTTP daemon: JSON-RPC 2.0 calls on `POST /api/v1/rpc`,
//! the health check `GET /api/v1/check`, and attachments fetched with
//! `getAttachment`. A recipient that rate-limits the account reports
//! `RATE_LIMIT_FAILURE` with `retryAfterSeconds`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use pmt_gateway::messaging::SendError;
use serde_json::{Value, json};

/// A client of one daemon and account.
#[derive(Clone)]
pub(crate) struct Rpc {
    pub(crate) client: reqwest::Client,
    /// `http://127.0.0.1:8080`, no trailing slash.
    pub(crate) url: String,
    pub(crate) account: String,
    pub(crate) ids: Arc<AtomicU64>,
}

impl std::fmt::Debug for Rpc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rpc")
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

impl Rpc {
    /// Whether the daemon answers its health check.
    pub(crate) async fn check(&self) -> Result<(), String> {
        let answer = self
            .client
            .get(format!("{}/api/v1/check", self.url))
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| e.without_url().to_string())?;
        if answer.status().is_success() {
            Ok(())
        } else {
            Err(format!("health check answered {}", answer.status()))
        }
    }

    /// Calls `method` for the account with `params`.
    pub(crate) async fn call(&self, method: &str, params: &Value) -> Result<Value, SendError> {
        let mut params = params.clone();
        params["account"] = json!(self.account);
        let id = self.ids.fetch_add(1, Ordering::Relaxed);
        let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let response = self
            .client
            .post(format!("{}/api/v1/rpc", self.url))
            .header("content-type", "application/json")
            .body(body.to_string())
            .timeout(CALL_TIMEOUT)
            .send()
            .await
            .map_err(transport)?;
        let status = response.status().as_u16();
        let text = response.text().await.map_err(transport)?;
        if status >= 500 {
            return Err(SendError::Server(format!("{status}")));
        }
        let answer: Value = serde_json::from_str(&text)
            .map_err(|_| SendError::Server(format!("{status}: not a JSON-RPC answer")))?;
        if answer["error"].is_object() {
            return Err(refusal(&answer["error"]));
        }
        let result = answer["result"].clone();
        match failed(&result["results"]) {
            Some(err) => Err(err),
            None => Ok(result),
        }
    }
}

/// The send error of a per-recipient failure list where no recipient got
/// the message.
fn failed(results: &Value) -> Option<SendError> {
    let results = results.as_array().filter(|r| !r.is_empty())?;
    if results
        .iter()
        .any(|r| r["type"].as_str() == Some("SUCCESS"))
    {
        return None;
    }
    let first = &results[0];
    Some(match first["type"].as_str().unwrap_or_default() {
        "RATE_LIMIT_FAILURE" => SendError::RateLimited {
            retry_after: first["retryAfterSeconds"].as_u64().map(Duration::from_secs),
        },
        "NETWORK_FAILURE" => SendError::Network("Signal could not be reached".to_owned()),
        other => SendError::Permanent(other.to_owned()),
    })
}

/// The send error of a JSON-RPC error.
pub(crate) fn refusal(error: &Value) -> SendError {
    if let Some(err) = failed(&error["data"]["response"]["results"]) {
        return err;
    }
    let message = error["message"].as_str().unwrap_or("request failed");
    let lower = message.to_lowercase();
    if lower.contains("rate limit") {
        return SendError::RateLimited { retry_after: None };
    }
    // -32603: the daemon itself failed.
    if error["code"].as_i64() == Some(-32603) {
        return SendError::Server(message.to_owned());
    }
    SendError::Permanent(message.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusals_map_onto_send_errors() {
        let limited = json!({"code": -5, "message": "Failed to send message",
            "data": {"response": {"results": [{"type": "RATE_LIMIT_FAILURE", "retryAfterSeconds": 7}]}}});
        assert_eq!(
            refusal(&limited),
            SendError::RateLimited {
                retry_after: Some(Duration::from_secs(7))
            }
        );
        assert!(matches!(
            refusal(&json!({"code": -32603, "message": "boom"})),
            SendError::Server(_)
        ));
        assert!(matches!(
            refusal(&json!({"code": -1, "message": "Unregistered user"})),
            SendError::Permanent(_)
        ));
        let partial = json!([{"type": "SUCCESS"}, {"type": "UNREGISTERED_FAILURE"}]);
        assert_eq!(failed(&partial), None);
    }
}

//! Discord REST calls (`/api/v10`), authorized with the bot token; a
//! refusal maps onto the gateway's send errors (`429` with its
//! `retry_after`, `5xx`, anything else permanent; a timeout after the
//! request left is `ambiguous`). The token is never logged.

use std::time::Duration;

use pmt_gateway::messaging::{MediaStream, SendError};
use serde_json::Value;

/// Where Discord lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    /// The REST base (`https://discord.com/api/v10`).
    pub api: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            api: "https://discord.com/api/v10".to_owned(),
        }
    }
}

/// A REST client for one bot token.
#[derive(Clone)]
pub(crate) struct Api {
    pub(crate) client: reqwest::Client,
    pub(crate) endpoints: Endpoints,
    pub(crate) token: String,
}

impl std::fmt::Debug for Api {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Api")
            .field("endpoints", &self.endpoints)
            .finish_non_exhaustive()
    }
}

/// A request body.
pub(crate) enum Body {
    /// None.
    Empty,
    /// JSON.
    Json(Value),
    /// A multipart form (attachments) with its content type.
    Form(String, Vec<u8>),
}

impl Api {
    /// `method path` with `body`; the answer's JSON (`null` for 204).
    pub(crate) async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Body,
    ) -> Result<Value, SendError> {
        let mut request = self
            .client
            .request(method, format!("{}{path}", self.endpoints.api))
            .header("authorization", format!("Bot {}", self.token))
            .timeout(Duration::from_secs(30));
        request = match body {
            Body::Empty => request,
            Body::Json(value) => request
                .header("content-type", "application/json")
                .body(value.to_string()),
            Body::Form(kind, bytes) => request.header("content-type", kind).body(bytes),
        };
        let response = request.send().await.map_err(transport)?;
        let status = response.status().as_u16();
        let header_wait = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<f64>().ok());
        let text = response.text().await.map_err(transport)?;
        let body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if (200..300).contains(&status) {
            return Ok(body);
        }
        let retry_after = body["retry_after"].as_f64().or(header_wait);
        Err(status_error(status, &body, retry_after))
    }

    /// Opens a CDN file for download.
    pub(crate) async fn download(&self, url: &str) -> Result<MediaStream, SendError> {
        let response = self.client.get(url).send().await.map_err(transport)?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            return Err(status_error(status, &Value::Null, None));
        }
        let declared_len = response.content_length();
        let chunks = futures_util::stream::unfold(response, |mut response| async move {
            match response.chunk().await {
                Ok(Some(chunk)) => Some((Ok(chunk.to_vec()), response)),
                Ok(None) => None,
                Err(err) => Some((Err(err.to_string()), response)),
            }
        });
        Ok(MediaStream {
            declared_len,
            chunks: Box::pin(chunks),
        })
    }
}

fn transport(err: reqwest::Error) -> SendError {
    if err.is_timeout() {
        SendError::Ambiguous(err.without_url().to_string())
    } else {
        SendError::Network(err.without_url().to_string())
    }
}

/// The send error of a Discord refusal.
pub(crate) fn status_error(status: u16, body: &Value, retry_after: Option<f64>) -> SendError {
    let message = body["message"].as_str().unwrap_or("request failed");
    match status {
        429 => SendError::RateLimited {
            retry_after: retry_after.map(|secs| Duration::from_secs_f64(secs.max(0.0).ceil())),
        },
        500.. => SendError::Server(format!("{status} {message}")),
        _ => SendError::Permanent(format!("{status} {message}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn refusals_map_onto_send_errors() {
        let limited = status_error(429, &json!({"retry_after": 6.2}), Some(6.2));
        assert_eq!(
            limited,
            SendError::RateLimited {
                retry_after: Some(Duration::from_secs(7))
            }
        );
        assert!(matches!(
            status_error(502, &json!({}), None),
            SendError::Server(_)
        ));
        let invalid = status_error(
            400,
            &json!({"code": 50035, "message": "Invalid Form Body"}),
            None,
        );
        assert_eq!(
            invalid,
            SendError::Permanent("400 Invalid Form Body".into())
        );
    }
}

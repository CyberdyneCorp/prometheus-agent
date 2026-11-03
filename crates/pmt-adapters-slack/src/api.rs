//! Slack Web API calls: `POST <api>/<method>` with a bearer token and a
//! JSON body. Slack answers HTTP 200 with `{"ok": false, "error": ...}`
//! for most refusals; a `429` carries `Retry-After`. The token is only a
//! header, never logged.

use std::path::Path;
use std::time::Duration;

use pmt_gateway::messaging::{MediaStream, SendError};
use serde_json::Value;

/// Where the Slack API lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    /// The Web API base (`https://slack.com/api`).
    pub api: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            api: "https://slack.com/api".to_owned(),
        }
    }
}

/// A Web API client for one token.
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

const CALL_TIMEOUT: Duration = Duration::from_secs(30);

impl Api {
    /// Calls `method` with a JSON body.
    pub(crate) async fn call(&self, method: &str, body: &Value) -> Result<Value, SendError> {
        let request = self
            .client
            .post(format!("{}/{method}", self.endpoints.api))
            .bearer_auth(&self.token)
            .header("content-type", "application/json; charset=utf-8")
            .body(body.to_string())
            .timeout(CALL_TIMEOUT);
        answer(request.send().await).await
    }

    /// Uploads `path` into `channel` (thread `thread`) with Slack's
    /// external upload flow: reserve an upload URL, send the bytes there,
    /// then complete the upload into the conversation.
    pub(crate) async fn upload(
        &self,
        channel: &str,
        thread: Option<&str>,
        path: &Path,
    ) -> Result<(), SendError> {
        let bytes = std::fs::read(path)
            .map_err(|e| SendError::Permanent(format!("{}: {e}", path.display())))?;
        let name = path
            .file_name()
            .map_or_else(|| "file".to_owned(), |n| n.to_string_lossy().into_owned());
        let reserved = self
            .call(
                "files.getUploadURLExternal",
                &serde_json::json!({"filename": name, "length": bytes.len()}),
            )
            .await?;
        let (Some(url), Some(id)) = (
            reserved["upload_url"].as_str(),
            reserved["file_id"].as_str(),
        ) else {
            return Err(SendError::Server("no upload URL in the answer".to_owned()));
        };
        let sent = self
            .client
            .post(url)
            .body(bytes)
            .timeout(Duration::from_mins(5))
            .send()
            .await
            .map_err(transport)?;
        if !sent.status().is_success() {
            return Err(status_error(sent.status().as_u16(), "upload failed", None));
        }
        let mut complete = serde_json::json!({
            "files": [{"id": id, "title": name}],
            "channel_id": channel,
        });
        if let Some(thread) = thread {
            complete["thread_ts"] = Value::from(thread);
        }
        self.call("files.completeUploadExternal", &complete)
            .await
            .map(drop)
    }

    /// Opens a private file URL with the bot token.
    pub(crate) async fn download(&self, url: &str) -> Result<MediaStream, SendError> {
        let response = self
            .client
            .get(url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(transport)?;
        if !response.status().is_success() {
            return Err(status_error(
                response.status().as_u16(),
                "download failed",
                None,
            ));
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
    if status == 200 && body["ok"].as_bool() == Some(true) {
        return Ok(body);
    }
    let error = body["error"].as_str().unwrap_or("request failed");
    Err(status_error(status, error, retry_after))
}

/// The send error of a Slack refusal (`status` 200 with `ok: false`, or
/// an HTTP error).
pub(crate) fn status_error(status: u16, error: &str, retry_after: Option<u64>) -> SendError {
    match (status, error) {
        (429, _) | (_, "ratelimited") => SendError::RateLimited {
            retry_after: retry_after.map(Duration::from_secs),
        },
        (500.., _) | (_, "internal_error" | "fatal_error" | "service_unavailable") => {
            SendError::Server(format!("{status} {error}"))
        }
        (_, "invalid_blocks" | "invalid_blocks_format") => SendError::Formatting(error.to_owned()),
        _ => SendError::Permanent(format!("{status} {error}")),
    }
}

/// Whether a refusal means the token is bad.
pub(crate) fn unauthorized(err: &SendError) -> bool {
    matches!(err, SendError::Permanent(why) if ["invalid_auth", "not_authed", "account_inactive", "token_revoked", "token_expired"]
        .iter()
        .any(|code| why.ends_with(code)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusals_map_onto_send_errors() {
        assert_eq!(
            status_error(429, "", Some(7)),
            SendError::RateLimited {
                retry_after: Some(Duration::from_secs(7))
            }
        );
        assert!(matches!(status_error(503, "", None), SendError::Server(_)));
        assert!(matches!(
            status_error(200, "invalid_blocks", None),
            SendError::Formatting(_)
        ));
        let denied = status_error(200, "invalid_auth", None);
        assert!(unauthorized(&denied));
        assert!(!unauthorized(&status_error(200, "channel_not_found", None)));
    }
}

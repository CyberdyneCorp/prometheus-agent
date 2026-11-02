//! Matrix client-server API calls with the access token as a bearer
//! header (never logged). Refusals carry an `errcode`: `M_LIMIT_EXCEEDED`
//! with `retry_after_ms` is throttling, `M_UNKNOWN_TOKEN` a bad token.

use std::path::Path;
use std::time::Duration;

use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use pmt_gateway::messaging::{MediaStream, SendError};
use serde_json::Value;

/// The client-server API of one homeserver and token.
#[derive(Clone)]
pub(crate) struct Api {
    pub(crate) client: reqwest::Client,
    /// `https://matrix.example.org`, no trailing slash.
    pub(crate) homeserver: String,
    pub(crate) token: String,
}

impl std::fmt::Debug for Api {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Api")
            .field("homeserver", &self.homeserver)
            .finish_non_exhaustive()
    }
}

const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// One path segment, escaped (room ids hold `!` and `:`).
pub(crate) fn segment(raw: &str) -> String {
    utf8_percent_encode(raw, NON_ALPHANUMERIC).to_string()
}

impl Api {
    fn url(&self, path: &str) -> String {
        format!("{}/_matrix/client/v3{path}", self.homeserver)
    }

    /// `GET` a client API path.
    pub(crate) async fn get(&self, path: &str) -> Result<Value, SendError> {
        self.get_within(path, CALL_TIMEOUT).await
    }

    /// `GET`, waiting at most `timeout` (a long `/sync` waits longer).
    pub(crate) async fn get_within(
        &self,
        path: &str,
        timeout: Duration,
    ) -> Result<Value, SendError> {
        let request = self
            .client
            .get(self.url(path))
            .bearer_auth(&self.token)
            .timeout(timeout);
        answer(request.send().await).await
    }

    /// `PUT` (sends are idempotent by transaction id) or `POST` JSON.
    pub(crate) async fn put(&self, path: &str, body: &Value) -> Result<Value, SendError> {
        let request = self
            .client
            .put(self.url(path))
            .bearer_auth(&self.token)
            .header("content-type", "application/json")
            .body(body.to_string())
            .timeout(CALL_TIMEOUT);
        answer(request.send().await).await
    }

    /// `POST` JSON.
    pub(crate) async fn post(&self, path: &str, body: &Value) -> Result<Value, SendError> {
        let request = self
            .client
            .post(self.url(path))
            .bearer_auth(&self.token)
            .header("content-type", "application/json")
            .body(body.to_string())
            .timeout(CALL_TIMEOUT);
        answer(request.send().await).await
    }

    /// Uploads a file to the media repository; its `mxc://` URI.
    pub(crate) async fn upload(&self, path: &Path, mime: &str) -> Result<String, SendError> {
        let bytes = std::fs::read(path)
            .map_err(|e| SendError::Permanent(format!("{}: {e}", path.display())))?;
        let name = path
            .file_name()
            .map_or_else(|| "file".to_owned(), |n| n.to_string_lossy().into_owned());
        let url = format!(
            "{}/_matrix/media/v3/upload?filename={}",
            self.homeserver,
            segment(&name)
        );
        let request = self
            .client
            .post(url)
            .bearer_auth(&self.token)
            .header("content-type", mime)
            .body(bytes)
            .timeout(Duration::from_mins(5));
        let uploaded = answer(request.send().await).await?;
        uploaded["content_uri"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| SendError::Server("the upload returned no content URI".to_owned()))
    }

    /// Opens an `mxc://server/id` file (authenticated media).
    pub(crate) async fn download(&self, mxc: &str) -> Result<MediaStream, SendError> {
        let Some((server, id)) = mxc.strip_prefix("mxc://").and_then(|r| r.split_once('/')) else {
            return Err(SendError::Permanent(format!("'{mxc}' is not an mxc URI")));
        };
        let url = format!(
            "{}/_matrix/client/v1/media/download/{}/{}",
            self.homeserver,
            segment(server),
            segment(id)
        );
        let response = self
            .client
            .get(url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(transport)?;
        if !response.status().is_success() {
            return Err(refusal(response.status().as_u16(), &Value::Null));
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
    let text = response.text().await.map_err(transport)?;
    let body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if (200..300).contains(&status) {
        return Ok(body);
    }
    Err(refusal(status, &body))
}

/// The send error of a refusal.
pub(crate) fn refusal(status: u16, body: &Value) -> SendError {
    let code = body["errcode"].as_str().unwrap_or_default();
    let error = body["error"].as_str().unwrap_or("request failed");
    if status == 429 || code == "M_LIMIT_EXCEEDED" {
        let wait = body["retry_after_ms"]
            .as_u64()
            .or_else(|| body["error"]["retry_after_ms"].as_u64());
        return SendError::RateLimited {
            retry_after: wait.map(Duration::from_millis),
        };
    }
    if status >= 500 {
        return SendError::Server(format!("{status} {error}"));
    }
    SendError::Permanent(format!("{status} {code} {error}"))
}

/// Whether a refusal means the access token is bad.
pub(crate) fn unauthorized(err: &SendError) -> bool {
    matches!(err, SendError::Permanent(why) if why.contains("M_UNKNOWN_TOKEN") || why.starts_with("401"))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn refusals_map_onto_send_errors() {
        assert_eq!(
            refusal(
                429,
                &json!({"errcode": "M_LIMIT_EXCEEDED", "retry_after_ms": 7000})
            ),
            SendError::RateLimited {
                retry_after: Some(Duration::from_secs(7))
            }
        );
        assert!(matches!(refusal(502, &Value::Null), SendError::Server(_)));
        let bad = refusal(401, &json!({"errcode": "M_UNKNOWN_TOKEN"}));
        assert!(unauthorized(&bad));
        assert!(!unauthorized(&refusal(
            403,
            &json!({"errcode": "M_FORBIDDEN"})
        )));
    }
}

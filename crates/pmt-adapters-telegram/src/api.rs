//! Bot API calls: one JSON `POST /bot<token>/<method>` per call, the
//! answer's `ok`/`error_code` mapped onto the gateway's send errors (a
//! `429` carries `parameters.retry_after`; malformed entities are a
//! `formatting` refusal; a timeout after the request left is `ambiguous`).
//! The token is only ever part of the URL, never logged.

use std::path::Path;
use std::time::Duration;

use pmt_gateway::messaging::multipart::Form;
use pmt_gateway::messaging::{MediaStream, SendError};
use serde_json::Value;

/// Where the Bot API lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    /// The API base (`https://api.telegram.org`).
    pub api: String,
    /// The file download base (`https://api.telegram.org/file`).
    pub files: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            api: "https://api.telegram.org".to_owned(),
            files: "https://api.telegram.org/file".to_owned(),
        }
    }
}

/// A Bot API client for one token.
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

/// How long one ordinary call may take.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

impl Api {
    fn url(&self, method: &str) -> String {
        format!("{}/bot{}/{method}", self.endpoints.api, self.token)
    }

    /// Calls `method` with a JSON body.
    pub(crate) async fn call(&self, method: &str, body: &Value) -> Result<Value, SendError> {
        self.call_within(method, body, CALL_TIMEOUT).await
    }

    /// Calls `method`, waiting at most `timeout` (long polls wait longer).
    pub(crate) async fn call_within(
        &self,
        method: &str,
        body: &Value,
        timeout: Duration,
    ) -> Result<Value, SendError> {
        let request = self
            .client
            .post(self.url(method))
            .header("content-type", "application/json")
            .body(body.to_string())
            .timeout(timeout);
        answer(request.send().await).await
    }

    /// Uploads a file with `method` (`sendPhoto`, `sendDocument`, ...).
    pub(crate) async fn upload(
        &self,
        method: &str,
        fields: &[(&str, String)],
        file_field: &str,
        path: &Path,
    ) -> Result<Value, SendError> {
        let bytes = std::fs::read(path)
            .map_err(|e| SendError::Permanent(format!("{}: {e}", path.display())))?;
        let name = path
            .file_name()
            .map_or_else(|| "file".to_owned(), |n| n.to_string_lossy().into_owned());
        let mut form = Form::new();
        for (key, value) in fields {
            form.text(key, value);
        }
        form.file(file_field, &name, &bytes);
        let request = self
            .client
            .post(self.url(method))
            .header("content-type", form.content_type())
            .body(form.finish())
            .timeout(Duration::from_mins(5));
        answer(request.send().await).await
    }

    /// Opens a file the Bot API stores (`getFile`, then its download, or
    /// a read of the path a `--local` server returns), refusing one over
    /// `files.limit` before reading it.
    pub(crate) async fn download(
        &self,
        file_id: &str,
        files: &FileAccess,
    ) -> Result<MediaStream, SendError> {
        let file = self
            .call("getFile", &serde_json::json!({"file_id": file_id}))
            .await?;
        if let Some(size) = file["file_size"].as_u64()
            && size > files.limit
        {
            return Err(SendError::Permanent(files.oversize(size)));
        }
        let path = file["file_path"]
            .as_str()
            .ok_or_else(|| SendError::Permanent("the file has no download path".to_owned()))?;
        if files.local && Path::new(path).is_absolute() {
            return local_file(path, file["file_size"].as_u64()).await;
        }
        let url = format!("{}/bot{}/{path}", self.endpoints.files, self.token);
        let response = self.client.get(url).send().await.map_err(transport)?;
        if !response.status().is_success() {
            return Err(status_error(
                response.status().as_u16(),
                "download failed",
                None,
            ));
        }
        let declared_len = response
            .content_length()
            .or_else(|| file["file_size"].as_u64());
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

/// The public Bot API serves bots files up to 20 MB.
pub(crate) const PUBLIC_FILE_LIMIT: u64 = 20 * 1024 * 1024;
/// A self-hosted Bot API server serves them up to 2048 MB.
pub(crate) const LOCAL_SERVER_FILE_LIMIT: u64 = 2048 * 1024 * 1024;

/// How inbound files are read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileAccess {
    /// The effective maximum: the server's, lowered by
    /// `gateway.media.max_inbound_bytes`.
    pub(crate) limit: u64,
    /// A self-hosted server (its limit is the larger one).
    pub(crate) self_hosted: bool,
    /// `--local` mode: read the absolute path the server returns.
    pub(crate) local: bool,
}

impl FileAccess {
    /// The access of a server (`self_hosted` or the public one) under the
    /// gateway's inbound cap (`0`: none).
    pub(crate) fn new(self_hosted: bool, local: bool, gateway_cap: u64) -> Self {
        let server = if self_hosted {
            LOCAL_SERVER_FILE_LIMIT
        } else {
            PUBLIC_FILE_LIMIT
        };
        let limit = if gateway_cap == 0 {
            server
        } else {
            server.min(gateway_cap)
        };
        Self {
            limit,
            self_hosted,
            local,
        }
    }

    /// What the user is told about a file of `size` bytes.
    pub(crate) fn oversize(&self, size: u64) -> String {
        let mb = |b: u64| b.div_ceil(1024 * 1024);
        let hint = if self.self_hosted || self.limit < PUBLIC_FILE_LIMIT {
            ""
        } else {
            "; a self-hosted Bot API server (options.api_base_url) raises Telegram's limit to 2048 MB"
        };
        format!(
            "the file is {} MB, over the {} MB maximum{hint}",
            mb(size),
            mb(self.limit)
        )
    }
}

/// A file a `--local` Bot API server stored at `path`.
async fn local_file(path: &str, declared: Option<u64>) -> Result<MediaStream, SendError> {
    let unreadable = |e: std::io::Error| {
        SendError::Permanent(format!("local Bot API file not readable: {path} ({e})"))
    };
    let file = tokio::fs::File::open(path).await.map_err(unreadable)?;
    let declared_len = match declared {
        Some(len) => Some(len),
        None => Some(file.metadata().await.map_err(unreadable)?.len()),
    };
    let chunks = futures_util::stream::unfold(file, |mut file| async move {
        use tokio::io::AsyncReadExt as _;
        let mut buf = vec![0; 64 * 1024];
        match file.read(&mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((Ok(buf), file))
            }
            Err(e) => Some((Err(e.to_string()), file)),
        }
    });
    Ok(MediaStream {
        declared_len,
        chunks: Box::pin(chunks),
    })
}

/// Logs the bot out of the public Bot API server, so a self-hosted one
/// may serve it (Telegram's `logOut`; once per migration).
///
/// # Errors
///
/// What the server answered.
pub async fn log_out(
    client: &reqwest::Client,
    endpoints: &Endpoints,
    token: &str,
) -> Result<(), SendError> {
    let api = Api {
        client: client.clone(),
        endpoints: endpoints.clone(),
        token: token.to_owned(),
    };
    api.call("logOut", &serde_json::json!({})).await.map(|_| ())
}

/// A transport error: before the request left it is `network`; a
/// timeout may come after it left (`ambiguous`).
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
    if body["ok"].as_bool() == Some(true) {
        return Ok(body["result"].clone());
    }
    let code = body["error_code"]
        .as_u64()
        .and_then(|c| u16::try_from(c).ok())
        .unwrap_or(status);
    let description = body["description"].as_str().unwrap_or("request failed");
    let retry_after = body["parameters"]["retry_after"].as_u64();
    Err(status_error(code, description, retry_after))
}

/// The send error of a Bot API refusal.
pub(crate) fn status_error(code: u16, description: &str, retry_after: Option<u64>) -> SendError {
    match code {
        429 => SendError::RateLimited {
            retry_after: retry_after.map(Duration::from_secs),
        },
        500.. => SendError::Server(format!("{code} {description}")),
        400 if description.contains("can't parse entities") => {
            SendError::Formatting(description.to_owned())
        }
        _ => SendError::Permanent(format!("{code} {description}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusals_map_onto_send_errors() {
        assert_eq!(
            status_error(429, "Too Many Requests", Some(300)),
            SendError::RateLimited {
                retry_after: Some(Duration::from_mins(5))
            }
        );
        assert!(matches!(
            status_error(502, "Bad Gateway", None),
            SendError::Server(_)
        ));
        assert!(matches!(
            status_error(400, "Bad Request: can't parse entities: x", None),
            SendError::Formatting(_)
        ));
        assert!(matches!(
            status_error(400, "Bad Request: message is too long", None),
            SendError::Permanent(_)
        ));
        assert!(matches!(
            status_error(403, "Forbidden", None),
            SendError::Permanent(_)
        ));
    }
}

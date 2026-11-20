//! Gateway proxy mode (spec: api-server, Gateway proxy mode): with
//! `gateway.proxy.url` set, a profile's adapters keep authorization,
//! pairing, routing keys and delivery here and every admitted turn runs on
//! the remote API server as a session-chat request keyed by the local
//! conversation key (`X-Prometheus-Session-Key`). The remote session of
//! each conversation is created on its first turn and remembered in
//! `<profile>/gateway/proxy-sessions.json`; one the remote no longer has
//! is created again.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{Value, json};

/// What a person sees when the remote cannot be reached.
pub(crate) const UNAVAILABLE: &str = "Upstream agent unavailable, try again shortly.";
/// The adapter error code of a refused key.
pub(crate) const AUTH_CODE: &str = "proxy_auth";

/// One profile's remote API server.
#[derive(Debug, Clone)]
pub(crate) struct Upstream {
    pub base: String,
    pub key: Option<String>,
    pub sessions: PathBuf,
    /// `None` when no client could be built (the turn fails as
    /// unreachable).
    pub client: Option<reqwest::Client>,
}

/// Why a forwarded turn produced no answer.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    /// The remote answered 401: the key is wrong.
    Auth,
    /// No connection, or the remote is not serving.
    Unreachable,
    /// The remote ran the turn and it failed.
    Failed(String),
}

/// Serialises updates of the remembered sessions.
static SESSIONS: Mutex<()> = Mutex::new(());

impl Upstream {
    /// The upstream of a profile at `dir` configured with `url`.
    pub(crate) fn new(url: &str, key: Option<String>, dir: &Path) -> Self {
        Self {
            base: url.trim_end_matches('/').to_owned(),
            key,
            sessions: dir.join("gateway").join("proxy-sessions.json"),
            client: pmt_net::http::client_builder(&pmt_net::OutboundPolicy::default())
                .and_then(|builder| {
                    builder.build().map_err(|e| {
                        pmt_core::Error::new(
                            &pmt_core::error::codes::NET_CLIENT_INIT,
                            e.to_string(),
                        )
                    })
                })
                .inspect_err(|err| tracing::warn!(error = %err, "no client for gateway.proxy"))
                .ok(),
        }
    }

    /// Runs one turn of conversation `key` with `text`, streaming the
    /// answer's pieces to `delta`; the final reply.
    pub(crate) async fn turn(
        &self,
        key: &str,
        text: &str,
        mut delta: impl FnMut(String) + Send,
    ) -> Result<Option<String>, Failure> {
        let client = self.client.as_ref().ok_or(Failure::Unreachable)?;
        let session = match self.remembered(key) {
            Some(session) => session,
            None => self.create(client, key).await?,
        };
        let response = if let Some(response) = self.chat(client, &session, key, text).await? {
            response
        } else {
            // Deleted on the remote: a new one continues the conversation.
            let session = self.create(client, key).await?;
            self.chat(client, &session, key, text)
                .await?
                .ok_or(Failure::Unreachable)?
        };
        read_stream(response, &mut delta).await
    }

    fn request(
        &self,
        client: &reqwest::Client,
        method: reqwest::Method,
        path: &str,
        key: &str,
    ) -> reqwest::RequestBuilder {
        let mut request = client
            .request(method, format!("{}{path}", self.base))
            .header("X-Prometheus-Session-Key", key);
        if let Some(bearer) = &self.key {
            request = request.bearer_auth(bearer);
        }
        request
    }

    async fn create(&self, client: &reqwest::Client, key: &str) -> Result<String, Failure> {
        let response = self
            .request(client, reqwest::Method::POST, "/api/sessions", key)
            .header("content-type", "application/json")
            .body(json!({}).to_string())
            .send()
            .await
            .map_err(|_| Failure::Unreachable)?;
        let response = checked(response)?;
        let text = response.text().await.map_err(|_| Failure::Unreachable)?;
        let body: Value = serde_json::from_str(&text).map_err(|_| Failure::Unreachable)?;
        let id = body["id"].as_str().ok_or(Failure::Unreachable)?.to_owned();
        self.remember(key, &id);
        Ok(id)
    }

    /// The streamed chat response; `None` when the session is gone (404).
    async fn chat(
        &self,
        client: &reqwest::Client,
        session: &str,
        key: &str,
        text: &str,
    ) -> Result<Option<reqwest::Response>, Failure> {
        let path = format!("/api/sessions/{session}/chat/stream");
        let response = self
            .request(client, reqwest::Method::POST, &path, key)
            .header("content-type", "application/json")
            .body(json!({"message": text}).to_string())
            .send()
            .await
            .map_err(|_| Failure::Unreachable)?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        checked(response).map(Some)
    }

    fn remembered(&self, key: &str) -> Option<String> {
        let _guard = SESSIONS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        read(&self.sessions).remove(key)
    }

    fn remember(&self, key: &str, session: &str) {
        let _guard = SESSIONS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut sessions = read(&self.sessions);
        sessions.insert(key.to_owned(), session.to_owned());
        let written = self
            .sessions
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| {
                let text = serde_json::to_string_pretty(&sessions).unwrap_or_default();
                std::fs::write(&self.sessions, text)
            });
        if let Err(err) = written {
            tracing::warn!(error = %err, "the remote session of a conversation was not remembered");
        }
    }
}

fn read(path: &Path) -> BTreeMap<String, String> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn checked(response: reqwest::Response) -> Result<reqwest::Response, Failure> {
    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(Failure::Auth);
    }
    if status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Err(Failure::Unreachable);
    }
    if !status.is_success() {
        return Err(Failure::Failed(format!(
            "the upstream agent answered {status}"
        )));
    }
    Ok(response)
}

/// Reads the session-chat event stream: `assistant.delta` pieces, then
/// `run.completed` (the reply) or `run.failed` / `run.cancelled`.
async fn read_stream(
    mut response: reqwest::Response,
    delta: &mut (impl FnMut(String) + Send),
) -> Result<Option<String>, Failure> {
    // Bytes, not text: a character may be split across chunks.
    let mut buffer: Vec<u8> = Vec::new();
    let mut event = String::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| Failure::Unreachable)? {
        buffer.extend_from_slice(&chunk);
        while let Some(end) = buffer.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = buffer.drain(..=end).collect();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end_matches(['\r', '\n']);
            if let Some(name) = line.strip_prefix("event:") {
                name.trim().clone_into(&mut event);
            } else if let Some(data) = line.strip_prefix("data:") {
                let data: Value = serde_json::from_str(data.trim()).unwrap_or_default();
                if let Some(done) = settle(&event, &data, delta) {
                    return done;
                }
            }
        }
    }
    Err(Failure::Unreachable)
}

/// One event; `Some` once the run ended.
fn settle(
    event: &str,
    data: &Value,
    delta: &mut (impl FnMut(String) + Send),
) -> Option<Result<Option<String>, Failure>> {
    match event {
        "assistant.delta" => {
            if let Some(text) = data["delta"].as_str() {
                delta(text.to_owned());
            }
            None
        }
        "run.completed" => Some(Ok(data["reply"].as_str().map(str::to_owned))),
        "run.failed" | "run.cancelled" => Some(Err(Failure::Failed(
            data["error"]
                .as_str()
                .unwrap_or("the upstream agent's turn failed")
                .to_owned(),
        ))),
        _ => None,
    }
}

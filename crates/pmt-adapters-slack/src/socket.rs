//! The Socket Mode connection: `apps.connections.open` with the app-level
//! token gives a WebSocket URL; Slack says `hello`, then sends envelopes
//! (events, interactions, slash commands) the adapter acknowledges by
//! envelope id. Slack keeps an envelope nobody acknowledged and delivers
//! it again on the next connection, so a dropped connection loses
//! nothing; a `disconnect` envelope (Slack rotating the socket) is a
//! reconnect like any other loss.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use pmt_gateway::messaging::{AdapterError, AdapterLink, AdapterState, ws};
use serde_json::{Value, json};
use tokio::sync::oneshot;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_util::sync::CancellationToken;

use crate::api::{self, Api};
use crate::normalize;

/// How long a connect waits for `hello`.
const HELLO_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) fn lost(code: &str, message: impl Into<String>, retryable: bool) -> AdapterError {
    AdapterError {
        retryable,
        code: code.to_owned(),
        message: message.into(),
    }
}

/// One connection's context.
pub(crate) struct Run {
    /// The client with the app-level token.
    pub(crate) app: Api,
    pub(crate) link: Arc<dyn AdapterLink>,
    pub(crate) bot_user: String,
    pub(crate) team: Option<String>,
    pub(crate) extra_ca: Option<PathBuf>,
    /// The proxy the WebSocket tunnels through, if any.
    pub(crate) proxy: Option<String>,
    pub(crate) stop: CancellationToken,
}

/// Opens Socket Mode; returns once Slack said `hello` (the connection
/// then runs in the background until it is lost or stopped).
pub(crate) async fn open(run: Run) -> Result<(), AdapterError> {
    let opened = run
        .app
        .call("apps.connections.open", &json!({}))
        .await
        .map_err(|e| {
            if api::unauthorized(&e) {
                lost("unauthorized", "Slack rejected the app-level token", false)
            } else {
                lost(
                    "slack_unreachable",
                    format!("Slack: {}", e.describe()),
                    true,
                )
            }
        })?;
    let url = opened["url"]
        .as_str()
        .ok_or_else(|| lost("slack_unreachable", "Slack gave no Socket Mode URL", true))?;
    let request = url.into_client_request().map_err(|e| {
        lost(
            "slack_unreachable",
            format!("bad Socket Mode URL: {e}"),
            true,
        )
    })?;
    let socket = ws::connect_via(request, run.extra_ca.as_deref(), run.proxy.as_deref())
        .await
        .map_err(|e| lost("slack_unreachable", e, true))?;
    let (ready_tx, ready_rx) = oneshot::channel();
    tokio::spawn(run.drive(socket, ready_tx));
    match tokio::time::timeout(HELLO_TIMEOUT, ready_rx).await {
        Ok(Ok(())) => Ok(()),
        _ => Err(lost(
            "slack_unreachable",
            "Slack did not open the Socket Mode session in time",
            true,
        )),
    }
}

impl Run {
    async fn drive(self, socket: ws::Socket, ready: oneshot::Sender<()>) {
        let (mut sink, mut source) = socket.split();
        let mut ready = Some(ready);
        let error = loop {
            let frame = tokio::select! {
                () = self.stop.cancelled() => {
                    let _ = sink.send(Message::Close(None)).await;
                    return;
                }
                frame = source.next() => frame,
            };
            let text = match frame {
                Some(Ok(Message::Text(text))) => text,
                Some(Ok(Message::Close(_)) | Err(_)) | None => {
                    break lost(
                        "slack_disconnected",
                        "Slack closed the Socket Mode connection",
                        true,
                    );
                }
                Some(Ok(_)) => continue,
            };
            let envelope: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            if let Some(id) = envelope["envelope_id"].as_str() {
                let ack = json!({"envelope_id": id}).to_string();
                if sink.send(Message::text(ack)).await.is_err() {
                    break lost(
                        "slack_disconnected",
                        "the acknowledgement could not be sent",
                        true,
                    );
                }
            }
            match envelope["type"].as_str() {
                Some("hello") => {
                    if let Some(ready) = ready.take() {
                        let _ = ready.send(());
                    }
                }
                Some("disconnect") => {
                    break lost(
                        "slack_disconnected",
                        "Slack is rotating the Socket Mode connection",
                        true,
                    );
                }
                _ => self.envelope(&envelope).await,
            }
        };
        if ready.is_none() && !self.stop.is_cancelled() {
            tracing::warn!(platform = crate::PLATFORM, "{}", error.message);
            self.link.report(AdapterState::Retrying, Some(error));
        }
    }

    async fn envelope(&self, envelope: &Value) {
        let payload = &envelope["payload"];
        let inbound = match envelope["type"].as_str() {
            Some("events_api") => {
                normalize::message(&payload["event"], self.team.as_deref(), &self.bot_user)
            }
            Some("interactive") => normalize::action(payload),
            Some("slash_commands") => {
                tracing::debug!(
                    platform = crate::PLATFORM,
                    "native slash commands are not dispatched yet; use the message form"
                );
                None
            }
            _ => None,
        };
        if let Some(inbound) = inbound {
            self.link.submit(inbound).await;
        }
    }
}

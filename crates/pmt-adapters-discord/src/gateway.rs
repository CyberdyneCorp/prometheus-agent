//! The Discord Gateway session: Hello, then Identify (or Resume, which
//! makes Discord replay every event the bot missed while disconnected),
//! heartbeats at the interval Discord asks for, and dispatches turned into
//! inbound events. A lost connection keeps the session for the next
//! connect; close codes that no reconnect can fix (bad token, the Message
//! Content intent not enabled) are fatal.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use pmt_gateway::messaging::{AdapterError, AdapterLink, AdapterState, ws};
use serde_json::{Value, json};
use tokio::sync::oneshot;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_util::sync::CancellationToken;

use crate::api::{Api, Body};
use crate::normalize;

/// `GUILDS | GUILD_MESSAGES | GUILD_MESSAGE_REACTIONS | DIRECT_MESSAGES |
/// MESSAGE_CONTENT`.
pub(crate) const INTENTS: u64 = 1 | 1 << 9 | 1 << 10 | 1 << 12 | 1 << 15;

/// How long a connect waits for READY or RESUMED.
const READY_TIMEOUT: Duration = Duration::from_secs(30);

/// What survives a reconnect.
#[derive(Debug, Default)]
pub(crate) struct Session {
    pub(crate) id: Option<String>,
    pub(crate) seq: Option<u64>,
    pub(crate) resume_url: Option<String>,
    pub(crate) bot_id: String,
}

/// One connection's context.
pub(crate) struct Run {
    pub(crate) api: Api,
    pub(crate) link: Arc<dyn AdapterLink>,
    pub(crate) session: Arc<Mutex<Session>>,
    pub(crate) require_mention: bool,
    pub(crate) extra_ca: Option<PathBuf>,
    pub(crate) stop: CancellationToken,
}

fn lost(code: &str, message: impl Into<String>, retryable: bool) -> AdapterError {
    AdapterError {
        retryable,
        code: code.to_owned(),
        message: message.into(),
    }
}

/// The error a close code stands for.
fn closed(code: u16, session: &Mutex<Session>) -> AdapterError {
    match code {
        4004 => lost("unauthorized", "Discord rejected the bot token", false),
        4014 => lost(
            "discord_intents",
            "Discord refused the connection: enable the Message Content intent for this bot in the Discord developer portal",
            false,
        ),
        4013 => lost(
            "discord_intents",
            "Discord refused the requested intents",
            false,
        ),
        4007 | 4009 => {
            // The session cannot be resumed: identify anew.
            let mut session = session.lock().expect("session poisoned");
            session.id = None;
            session.seq = None;
            lost(
                "discord_disconnected",
                format!("Discord closed the session ({code})"),
                true,
            )
        }
        _ => lost(
            "discord_disconnected",
            format!("Discord closed the connection ({code})"),
            true,
        ),
    }
}

/// Opens the Gateway at `url`; returns once the session is ready (the
/// connection then runs in the background until it is lost or stopped).
pub(crate) async fn open(run: Run, url: &str) -> Result<(), AdapterError> {
    let request = format!("{}/?v=10&encoding=json", url.trim_end_matches('/'))
        .into_client_request()
        .map_err(|e| {
            lost(
                "discord_unreachable",
                format!("bad gateway URL {url}: {e}"),
                true,
            )
        })?;
    let socket = ws::connect(request, run.extra_ca.as_deref())
        .await
        .map_err(|e| lost("discord_unreachable", e, true))?;
    let (ready_tx, ready_rx) = oneshot::channel();
    tokio::spawn(run.drive(socket, ready_tx));
    match tokio::time::timeout(READY_TIMEOUT, ready_rx).await {
        Ok(Ok(result)) => result,
        _ => Err(lost(
            "discord_unreachable",
            "Discord did not confirm the session in time",
            true,
        )),
    }
}

/// What a frame asks of the connection.
enum Flow {
    Continue,
    Ready,
    Heartbeat,
    Lost(AdapterError),
}

impl Run {
    fn identify(&self) -> Value {
        let session = self.session.lock().expect("session poisoned");
        match (&session.id, session.seq) {
            (Some(id), Some(seq)) => json!({"op": 6, "d": {
                "token": self.api.token, "session_id": id, "seq": seq}}),
            _ => json!({"op": 2, "d": {
                "token": self.api.token,
                "intents": INTENTS,
                "properties": {"os": std::env::consts::OS, "browser": "prometheus", "device": "prometheus"}
            }}),
        }
    }

    fn heartbeat(&self) -> Message {
        let seq = self.session.lock().expect("session poisoned").seq;
        Message::text(json!({"op": 1, "d": seq}).to_string())
    }

    async fn drive(self, socket: ws::Socket, ready: oneshot::Sender<Result<(), AdapterError>>) {
        let (mut sink, mut stream) = socket.split();
        let mut ready = Some(ready);
        let outcome = async {
            let interval = hello(&mut stream).await?;
            sink.send(Message::text(self.identify().to_string()))
                .await
                .map_err(|e| lost("discord_disconnected", e.to_string(), true))?;
            let mut ticker = tokio::time::interval(interval);
            loop {
                let frame = tokio::select! {
                    () = self.stop.cancelled() => {
                        let _ = sink.send(Message::Close(None)).await;
                        return Ok(());
                    }
                    _ = ticker.tick() => Flow::Heartbeat,
                    frame = stream.next() => self.frame(frame).await,
                };
                match frame {
                    Flow::Continue => {}
                    Flow::Ready => {
                        if let Some(ready) = ready.take() {
                            let _ = ready.send(Ok(()));
                        }
                    }
                    Flow::Heartbeat => sink
                        .send(self.heartbeat())
                        .await
                        .map_err(|e| lost("discord_disconnected", e.to_string(), true))?,
                    Flow::Lost(error) => return Err(error),
                }
            }
        }
        .await;
        let Err(error) = outcome else { return };
        if let Some(ready) = ready.take() {
            let _ = ready.send(Err(error));
        } else {
            let state = if error.retryable {
                AdapterState::Retrying
            } else {
                AdapterState::Fatal
            };
            tracing::warn!(platform = "discord", "connection lost: {}", error.message);
            self.link.report(state, Some(error));
        }
    }

    async fn frame(
        &self,
        frame: Option<Result<Message, tokio_tungstenite::tungstenite::Error>>,
    ) -> Flow {
        let text = match frame {
            Some(Ok(Message::Text(text))) => text,
            Some(Ok(Message::Close(close))) => {
                let code = close.map_or(1000, |c| u16::from(c.code));
                return Flow::Lost(closed(code, &self.session));
            }
            Some(Ok(_)) => return Flow::Continue,
            Some(Err(e)) => return Flow::Lost(lost("discord_disconnected", e.to_string(), true)),
            None => {
                return Flow::Lost(lost(
                    "discord_disconnected",
                    "Discord closed the connection",
                    true,
                ));
            }
        };
        let Ok(payload) = serde_json::from_str::<Value>(&text) else {
            return Flow::Continue;
        };
        if let Some(seq) = payload["s"].as_u64() {
            self.session.lock().expect("session poisoned").seq = Some(seq);
        }
        match payload["op"].as_u64() {
            Some(0) => self.dispatch(&payload).await,
            Some(1) => Flow::Heartbeat,
            Some(7) => Flow::Lost(lost(
                "discord_reconnect",
                "Discord asked to reconnect",
                true,
            )),
            Some(9) => {
                if payload["d"].as_bool() != Some(true) {
                    let mut session = self.session.lock().expect("session poisoned");
                    session.id = None;
                    session.seq = None;
                }
                Flow::Lost(lost(
                    "discord_disconnected",
                    "Discord invalidated the session",
                    true,
                ))
            }
            _ => Flow::Continue,
        }
    }

    async fn dispatch(&self, payload: &Value) -> Flow {
        let data = &payload["d"];
        match payload["t"].as_str().unwrap_or_default() {
            "READY" => {
                let mut session = self.session.lock().expect("session poisoned");
                session.id = data["session_id"].as_str().map(str::to_owned);
                session.resume_url = data["resume_gateway_url"].as_str().map(str::to_owned);
                if let Some(id) = data["user"]["id"].as_str() {
                    id.clone_into(&mut session.bot_id);
                }
                Flow::Ready
            }
            "RESUMED" => Flow::Ready,
            "MESSAGE_CREATE" => {
                let bot = self
                    .session
                    .lock()
                    .expect("session poisoned")
                    .bot_id
                    .clone();
                if let Some(event) = normalize::message(data, &bot, self.require_mention) {
                    self.link.submit(event).await;
                }
                Flow::Continue
            }
            "INTERACTION_CREATE" => {
                self.acknowledge(data).await;
                if let Some(event) = normalize::interaction(data) {
                    self.link.submit(event).await;
                }
                Flow::Continue
            }
            _ => Flow::Continue,
        }
    }

    /// Answers a button press so the user's client stops waiting (the
    /// card itself is updated by the gateway).
    async fn acknowledge(&self, interaction: &Value) {
        let (Some(id), Some(token)) = (interaction["id"].as_str(), interaction["token"].as_str())
        else {
            return;
        };
        let path = format!("/interactions/{id}/{token}/callback");
        let body = Body::Json(json!({"type": 6}));
        if let Err(err) = self.api.call(reqwest::Method::POST, &path, body).await {
            tracing::debug!("interaction not acknowledged: {}", err.describe());
        }
    }
}

/// Reads Hello; the heartbeat interval it asks for.
async fn hello(
    stream: &mut futures_util::stream::SplitStream<ws::Socket>,
) -> Result<Duration, AdapterError> {
    while let Some(frame) = stream.next().await {
        let frame = frame.map_err(|e| lost("discord_disconnected", e.to_string(), true))?;
        let Message::Text(text) = frame else { continue };
        let payload: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if payload["op"].as_u64() == Some(10) {
            let millis = payload["d"]["heartbeat_interval"]
                .as_u64()
                .unwrap_or(41_250);
            return Ok(Duration::from_millis(millis.max(100)));
        }
    }
    Err(lost(
        "discord_disconnected",
        "Discord closed the connection before Hello",
        true,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_codes_that_no_reconnect_fixes_are_fatal() {
        let session = Mutex::new(Session {
            id: Some("s".into()),
            seq: Some(3),
            ..Session::default()
        });
        let intents = closed(4014, &session);
        assert!(!intents.retryable);
        assert!(
            intents
                .message
                .contains("enable the Message Content intent")
        );
        assert!(!closed(4004, &session).retryable);
        assert!(closed(4000, &session).retryable);
        assert!(session.lock().unwrap().id.is_some());
        assert!(closed(4009, &session).retryable);
        assert!(session.lock().unwrap().id.is_none());
    }
}

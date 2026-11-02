//! A Matrix homeserver mock: `/sync` long polls over a server-side
//! timeline addressed by `since` tokens (so a client that reconnects with
//! its cursor misses nothing), sends limited to 32,000 UTF-8 bytes of
//! body, edits, reactions, redactions, typing, member lists, the media
//! repository and scripted failures.

// A fixture: a mock that cannot serve should fail loudly.
#![allow(clippy::missing_panics_doc)]

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use percent_encoding::percent_decode_str;
use pmt_gateway::messaging::conformance::{Failure, MockPlatform, MockSent};
use pmt_gateway::messaging::mock_server;
use pmt_gateway::messaging::{HttpRequest, HttpResponse};
use serde_json::{Value, json};
use tokio::sync::Notify;

/// The access token the mock accepts.
pub const TOKEN: &str = "syt_kit";
/// The two-member room the kit talks in.
pub const DM: &str = "!dm:kit";
/// The user the kit talks as.
pub const USER: &str = "@kit:kit";
/// The bot's user id.
pub const BOT: &str = "@bot:kit";
const LIMIT: usize = 32_000;

#[derive(Default)]
struct State {
    /// The DM room's timeline.
    timeline: Vec<Value>,
    last_event: Option<Value>,
    next: u64,
    sent: Vec<MockSent>,
    typing: usize,
    reactions: Vec<(String, String)>,
    redactions: usize,
    failures: VecDeque<Failure>,
    media: Vec<(String, Vec<u8>)>,
    fail_next_sync: bool,
    invites: Vec<(String, String)>,
    joined: Vec<String>,
}

/// The mock.
pub struct MatrixMock {
    state: Mutex<State>,
    arrived: Notify,
    addr: Mutex<Option<SocketAddr>>,
}

impl MatrixMock {
    /// Starts the homeserver on a free loopback port.
    pub async fn start() -> Arc<Self> {
        let mock = Arc::new(Self {
            state: Mutex::new(State {
                next: 100,
                ..State::default()
            }),
            arrived: Notify::new(),
            addr: Mutex::new(None),
        });
        let server = Arc::clone(&mock);
        let addr = mock_server::serve(move |request| {
            let server = Arc::clone(&server);
            async move { server.serve(&request).await }
        })
        .await
        .unwrap();
        *mock.addr.lock().unwrap() = Some(addr);
        mock
    }

    /// The homeserver URL.
    pub fn homeserver(&self) -> String {
        format!("http://{}", self.addr.lock().unwrap().expect("started"))
    }

    /// The table lines that point the adapter at this mock.
    pub fn table(&self) -> String {
        format!("options = {{ homeserver = \"{}\" }}", self.homeserver())
    }

    /// Stores a media file; its `mxc://` URI.
    pub fn add_media(&self, id: &str, bytes: Vec<u8>) -> String {
        self.lock().media.push((id.to_owned(), bytes));
        format!("mxc://kit/{id}")
    }

    /// `(room, inviter)` invites waiting, and rooms joined since.
    pub fn joined(&self) -> Vec<String> {
        self.lock().joined.clone()
    }

    /// The bot is invited to `room` by `inviter`.
    pub fn invite(&self, room: &str, inviter: &str) {
        self.lock()
            .invites
            .push((room.to_owned(), inviter.to_owned()));
        self.arrived.notify_waiters();
    }

    /// Redactions received.
    pub fn redactions(&self) -> usize {
        self.lock().redactions
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    fn id(&self) -> u64 {
        let mut state = self.lock();
        state.next += 1;
        state.next
    }

    /// Appends an event of `USER` to the DM's timeline.
    pub fn push(&self, content: &Value) -> String {
        let id = format!("${}", self.id());
        let event = json!({"type": "m.room.message", "event_id": id, "sender": USER,
                           "origin_server_ts": 1_700_000_000_000_i64, "content": content});
        let mut state = self.lock();
        state.timeline.push(event.clone());
        state.last_event = Some(event);
        drop(state);
        self.arrived.notify_waiters();
        id
    }

    async fn serve(&self, request: &HttpRequest) -> HttpResponse {
        if request
            .path
            .starts_with("/_matrix/client/v1/media/download/")
        {
            let id = request.path.rsplit('/').next().unwrap_or_default();
            let found = self.lock().media.iter().find(|(m, _)| m == id).cloned();
            return found.map_or_else(
                || error(404, "M_NOT_FOUND"),
                |(_, bytes)| mock_server::bytes(200, bytes),
            );
        }
        if request.header("authorization") != Some(&*format!("Bearer {TOKEN}")) {
            return error(401, "M_UNKNOWN_TOKEN");
        }
        if request.path == "/_matrix/media/v3/upload" {
            let id = format!("up{}", self.id());
            self.lock().media.push((id.clone(), request.body.clone()));
            return mock_server::json(200, &json!({"content_uri": format!("mxc://kit/{id}")}));
        }
        let Some(path) = request.path.strip_prefix("/_matrix/client/v3/") else {
            return error(404, "M_UNRECOGNIZED");
        };
        let parts: Vec<String> = path
            .split('/')
            .map(|p| percent_decode_str(p).decode_utf8_lossy().into_owned())
            .collect();
        let parts: Vec<&str> = parts.iter().map(String::as_str).collect();
        let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        match (request.method.as_str(), parts.as_slice()) {
            ("GET", ["account", "whoami"]) => mock_server::json(200, &json!({"user_id": BOT})),
            ("GET", ["sync"]) => {
                self.sync(request.query.as_deref().unwrap_or_default())
                    .await
            }
            ("PUT", ["rooms", room, "send", kind, _]) => self.send(room, kind, &body),
            ("PUT", ["rooms", _, "redact", _, _]) => {
                self.lock().redactions += 1;
                mock_server::json(200, &json!({"event_id": format!("${}", self.id())}))
            }
            ("PUT", ["rooms", _, "typing", _]) => {
                self.lock().typing += 1;
                mock_server::json(200, &json!({}))
            }
            ("GET", ["rooms", room, "joined_members"]) => {
                let members = if *room == DM {
                    json!({USER: {}, BOT: {}})
                } else {
                    json!({USER: {}, BOT: {}, "@other:kit": {}})
                };
                mock_server::json(200, &json!({"joined": members}))
            }
            ("GET", ["rooms", _, "state", "m.room.name"]) => error(404, "M_NOT_FOUND"),
            ("POST", ["join", room]) => {
                self.lock().joined.push((*room).to_owned());
                mock_server::json(200, &json!({"room_id": room}))
            }
            _ => error(404, "M_UNRECOGNIZED"),
        }
    }

    async fn sync(&self, query: &str) -> HttpResponse {
        let param = |name: &str| {
            query
                .split('&')
                .find_map(|kv| kv.strip_prefix(&format!("{name}=")))
                .map(|v| percent_decode_str(v).decode_utf8_lossy().into_owned())
        };
        let since: Option<usize> = param("since").and_then(|s| s.strip_prefix('s')?.parse().ok());
        let wait =
            Duration::from_millis(param("timeout").and_then(|t| t.parse().ok()).unwrap_or(0));
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let notified = self.arrived.notified();
            {
                let mut state = self.lock();
                if std::mem::take(&mut state.fail_next_sync) {
                    return error(502, "M_UNKNOWN");
                }
                let end = state.timeline.len();
                let Some(from) = since else {
                    // An initial sync: the cursor only.
                    return mock_server::json(200, &json!({"next_batch": format!("s{end}")}));
                };
                let invites = std::mem::take(&mut state.invites);
                if from < end || !invites.is_empty() {
                    return mock_server::json(200, &batch(&state.timeline[from..], end, &invites));
                }
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return mock_server::json(
                    200,
                    &json!({"next_batch": format!("s{}", since.unwrap_or(0))}),
                );
            }
        }
    }

    fn send(&self, room: &str, kind: &str, body: &Value) -> HttpResponse {
        let mut state = self.lock();
        if kind == "m.reaction" {
            let relation = &body["m.relates_to"];
            state.reactions.push((
                relation["event_id"].as_str().unwrap_or_default().to_owned(),
                relation["key"].as_str().unwrap_or_default().to_owned(),
            ));
            state.next += 1;
            return mock_server::json(200, &json!({"event_id": format!("${}", state.next)}));
        }
        if let Some(failure) = state.failures.pop_front() {
            return match failure {
                Failure::RateLimited(secs) => mock_server::json(
                    429,
                    &json!({"errcode": "M_LIMIT_EXCEEDED", "error": "Too Many Requests",
                            "retry_after_ms": secs * 1000}),
                ),
                Failure::ServerError => error(500, "M_UNKNOWN"),
                Failure::Markup | Failure::Forbidden => error(403, "M_FORBIDDEN"),
            };
        }
        let text = body["body"].as_str().unwrap_or_default();
        if text.len() > LIMIT {
            return error(413, "M_TOO_LARGE");
        }
        let relation = &body["m.relates_to"];
        if relation["rel_type"] == "m.replace" {
            let target = relation["event_id"].as_str().unwrap_or_default();
            let new = body["m.new_content"]["body"].as_str().unwrap_or_default();
            if let Some(message) = state.sent.iter_mut().find(|m| m.message_id == target) {
                new.clone_into(&mut message.text);
            }
        }
        state.next += 1;
        let id = format!("${}", state.next);
        state.sent.push(MockSent {
            chat_id: room.to_owned(),
            message_id: id.clone(),
            text: text.to_owned(),
            buttons: Vec::new(),
        });
        mock_server::json(200, &json!({"event_id": id}))
    }
}

fn batch(events: &[Value], end: usize, invites: &[(String, String)]) -> Value {
    let invite: serde_json::Map<String, Value> = invites
        .iter()
        .map(|(room, inviter)| {
            let state = json!({"events": [{"type": "m.room.member", "state_key": BOT,
                "sender": inviter, "content": {"membership": "invite"}}]});
            (room.clone(), json!({"invite_state": state}))
        })
        .collect();
    json!({
        "next_batch": format!("s{end}"),
        "rooms": {
            "join": {DM: {"summary": {"m.joined_member_count": 2},
                          "timeline": {"events": events}}},
            "invite": invite,
        }
    })
}

fn error(status: u16, code: &str) -> HttpResponse {
    mock_server::json(status, &json!({"errcode": code, "error": code}))
}

impl MockPlatform for MatrixMock {
    fn dm(&self) -> (String, String) {
        (DM.to_owned(), USER.to_owned())
    }

    fn inject(&self, text: &str) -> String {
        self.push(&json!({"msgtype": "m.text", "body": text}))
    }

    fn redeliver(&self) {
        let mut state = self.lock();
        if let Some(event) = state.last_event.clone() {
            state.timeline.push(event);
        }
        drop(state);
        self.arrived.notify_waiters();
    }

    fn drop_connection(&self) {
        self.lock().fail_next_sync = true;
        self.arrived.notify_waiters();
    }

    fn press(&self, _message_id: &str, _data: &str) {}

    fn fail_next(&self, failure: Failure) {
        self.lock().failures.push_back(failure);
    }

    fn reports_markup_errors(&self) -> bool {
        false
    }

    fn sent(&self) -> Vec<MockSent> {
        self.lock().sent.clone()
    }

    fn typing(&self) -> usize {
        self.lock().typing
    }

    fn reactions(&self) -> Vec<(String, String)> {
        self.lock().reactions.clone()
    }
}

/// The adapter, built for a fresh profile of the home at `home` that
/// holds the mock's token, and the mock it talks to: what
/// `prometheus plugins test-adapter matrix` and the crate's tests run the
/// conformance kit on.
///
/// # Errors
///
/// The profile cannot be set up.
pub async fn kit(
    home: &std::path::Path,
) -> Result<
    (
        Arc<dyn pmt_gateway::messaging::Adapter>,
        Arc<dyn MockPlatform>,
    ),
    pmt_core::error::Error,
> {
    let mock = MatrixMock::start().await;
    let setup = pmt_gateway::messaging::conformance::kit_setup(
        home,
        crate::PLATFORM,
        &mock.table(),
        &[(crate::TOKEN_SECRET, TOKEN)],
    )?;
    let adapter = crate::Matrix::new(&setup)?;
    Ok((Arc::new(adapter), mock))
}

//! A Matrix homeserver mock: `/sync` long polls over a server-side
//! timeline addressed by `since` tokens (so a client that reconnects with
//! its cursor misses nothing), sends limited to 32,000 UTF-8 bytes of
//! body, threads, edits, reactions, redactions, typing, member lists, the
//! media repository (files both ways) and scripted failures.

// A fixture: a mock that cannot serve should fail loudly.
#![allow(clippy::missing_panics_doc)]

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use percent_encoding::percent_decode_str;
use pmt_gateway::messaging::conformance::{
    Failure, MockFile, MockPlatform, MockSent, mime_of_name,
};
use pmt_gateway::messaging::mock_server;
use pmt_gateway::messaging::{HttpRequest, HttpResponse, MediaKind};
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
/// The bot's device (the access token's).
pub const BOT_DEVICE: &str = "BOTDEVICE";
/// A thread root in the DM.
pub const THREAD: &str = "$root";
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
    deleted: Vec<String>,
    received: Vec<MockFile>,
    failures: VecDeque<Failure>,
    media: Vec<(String, Vec<u8>)>,
    fail_next_sync: bool,
    invites: Vec<(String, String)>,
    joined: Vec<String>,
    /// Power levels by user (`m.room.power_levels` of every room).
    power: Vec<(String, i64)>,
    /// Reading power levels fails with 403.
    refuse_power: bool,
    /// The DM room is end-to-end encrypted.
    encrypted: bool,
    /// `/sync` requests served.
    syncs: usize,
    /// Every room event sent: (room, type, content).
    events: Vec<(String, String, Value)>,
}

/// The mock.
pub struct MatrixMock {
    state: Mutex<State>,
    keys: crate::mock_keys::Keys,
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
            keys: crate::mock_keys::Keys::default(),
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

    /// Gives `user` power level `level` in every room.
    pub fn set_power_level(&self, user: &str, level: i64) {
        self.lock().power.push((user.to_owned(), level));
    }

    /// Reading a room's power levels is refused.
    pub fn refuse_power_levels(&self) {
        self.lock().refuse_power = true;
    }

    fn power_levels(&self) -> HttpResponse {
        let state = self.lock();
        if state.refuse_power {
            return error(403, "M_FORBIDDEN");
        }
        let users: serde_json::Map<String, Value> = state
            .power
            .iter()
            .map(|(u, l)| (u.clone(), json!(l)))
            .collect();
        mock_server::json(200, &json!({"users": users, "users_default": 0}))
    }

    /// The homeserver's key store (end-to-end encryption).
    pub fn keys(&self) -> &crate::mock_keys::Keys {
        &self.keys
    }

    /// Turns end-to-end encryption on in the DM room.
    pub fn encrypt_dm(&self) {
        self.lock().encrypted = true;
    }

    /// Every room event the bot sent: (room, type, content).
    pub fn events(&self) -> Vec<(String, String, Value)> {
        self.lock().events.clone()
    }

    /// A stored media file's bytes.
    pub fn media(&self, mxc: &str) -> Option<Vec<u8>> {
        let id = mxc.strip_prefix("mxc://kit/")?;
        self.lock()
            .media
            .iter()
            .find(|(m, _)| m == id)
            .map(|(_, b)| b.clone())
    }

    /// `/sync` requests served so far.
    pub fn syncs(&self) -> usize {
        self.lock().syncs
    }

    /// Wakes `/sync` long polls (to-device events arrived).
    pub fn wake(&self) {
        self.arrived.notify_waiters();
    }

    /// Appends an `m.room.encrypted` event of `USER` with `content` to the
    /// DM's timeline.
    pub fn push_encrypted(&self, content: &Value) -> String {
        self.push_event("m.room.encrypted", content)
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
        self.push_event("m.room.message", content)
    }

    /// Delivers an encrypted event labelled as sent by `sender` (what a
    /// malicious homeserver could do with another user's session).
    pub fn push_encrypted_as(&self, sender: &str, content: &Value) -> String {
        self.push_event_as(sender, "m.room.encrypted", content)
    }

    fn push_event(&self, kind: &str, content: &Value) -> String {
        self.push_event_as(USER, kind, content)
    }

    fn push_event_as(&self, sender: &str, kind: &str, content: &Value) -> String {
        let id = format!("${}", self.id());
        let event = json!({"type": kind, "event_id": id, "sender": sender,
                           "origin_server_ts": jiff::Timestamp::now().as_millisecond(),
                           "content": content});
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
            let id = percent_decode_str(id).decode_utf8_lossy();
            let found = self.lock().media.iter().find(|(m, _)| *m == id).cloned();
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
        if let Some(answer) = self.encryption(&request.method, &parts, &body) {
            return answer;
        }
        match (request.method.as_str(), parts.as_slice()) {
            ("GET", ["account", "whoami"]) => {
                mock_server::json(200, &json!({"user_id": BOT, "device_id": BOT_DEVICE}))
            }
            ("GET", ["sync"]) => {
                self.sync(request.query.as_deref().unwrap_or_default())
                    .await
            }
            ("PUT", ["rooms", room, "send", kind, _]) => self.send(room, kind, &body),
            ("PUT", ["rooms", _, "redact", event, _]) => {
                let mut state = self.lock();
                state.redactions += 1;
                state.deleted.push((*event).to_owned());
                drop(state);
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
            ("GET", ["rooms", _, "state", "m.room.power_levels", ..]) => self.power_levels(),
            ("POST", ["createRoom"]) => {
                // The kit user's direct room is the DM room.
                let invited = body["invite"][0].as_str().unwrap_or_default();
                if invited == USER && body["is_direct"] == true {
                    mock_server::json(200, &json!({"room_id": DM}))
                } else {
                    error(403, "M_FORBIDDEN")
                }
            }
            ("POST", ["join", room]) => {
                self.lock().joined.push((*room).to_owned());
                mock_server::json(200, &json!({"room_id": room}))
            }
            _ => error(404, "M_UNRECOGNIZED"),
        }
    }

    /// The key endpoints and the DM's encryption state; `None` for other
    /// requests.
    fn encryption(&self, method: &str, parts: &[&str], body: &Value) -> Option<HttpResponse> {
        if let ("GET", ["rooms", room, "state", "m.room.encryption", ..]) = (method, parts) {
            return Some(if *room == DM && self.lock().encrypted {
                mock_server::json(200, &json!({"algorithm": "m.megolm.v1.aes-sha2"}))
            } else {
                error(404, "M_NOT_FOUND")
            });
        }
        let answer = self.keys.serve((BOT, BOT_DEVICE), method, parts, body)?;
        // To-device messages may wait for the bot's long poll.
        self.arrived.notify_waiters();
        Some(answer)
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
        self.lock().syncs += 1;
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
                let keys_waiting = self.keys.has_to_device(BOT, BOT_DEVICE);
                if from < end || !state.invites.is_empty() || keys_waiting {
                    let invites = std::mem::take(&mut state.invites);
                    let mut answer = batch(&state.timeline[from..], end, &invites);
                    answer["to_device"]["events"] =
                        json!(self.keys.take_to_device(BOT, BOT_DEVICE));
                    answer["device_one_time_keys_count"]["signed_curve25519"] =
                        json!(self.keys.one_time_key_count(BOT, BOT_DEVICE));
                    return mock_server::json(200, &answer);
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
        state
            .events
            .push((room.to_owned(), kind.to_owned(), body.clone()));
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
        let thread_id = (relation["rel_type"] == "m.thread")
            .then(|| relation["event_id"].as_str().map(str::to_owned))
            .flatten();
        if let Some(file) = received_file(&state.media, room, thread_id.clone(), body) {
            state.received.push(file);
        }
        state.next += 1;
        let id = format!("${}", state.next);
        state.sent.push(MockSent {
            chat_id: room.to_owned(),
            message_id: id.clone(),
            text: text.to_owned(),
            buttons: Vec::new(),
            thread_id,
        });
        mock_server::json(200, &json!({"event_id": id}))
    }
}

/// The file a media message shows, its content from the repository.
fn received_file(
    media: &[(String, Vec<u8>)],
    room: &str,
    thread_id: Option<String>,
    content: &Value,
) -> Option<MockFile> {
    let kind = match content["msgtype"].as_str()? {
        "m.image" => MediaKind::Image,
        "m.audio" if content["org.matrix.msc3245.voice"].is_object() => MediaKind::Voice,
        "m.audio" => MediaKind::Audio,
        "m.video" => MediaKind::Video,
        "m.file" => MediaKind::Document,
        _ => return None,
    };
    let id = content["url"].as_str()?.strip_prefix("mxc://kit/")?;
    let bytes = media.iter().find(|(m, _)| m == id)?.1.clone();
    Some(MockFile {
        chat_id: room.to_owned(),
        thread_id,
        name: content["body"].as_str().unwrap_or_default().to_owned(),
        kind,
        bytes,
    })
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

    fn thread(&self) -> Option<(String, String)> {
        Some((DM.to_owned(), THREAD.to_owned()))
    }

    fn inject_media(
        &self,
        kind: MediaKind,
        name: &str,
        bytes: &[u8],
        caption: &str,
    ) -> Option<String> {
        let uri = self.add_media(&format!("in-{}", name.replace('.', "_")), bytes.to_vec());
        let msgtype = match kind {
            MediaKind::Image => "m.image",
            MediaKind::Audio | MediaKind::Voice => "m.audio",
            MediaKind::Video => "m.video",
            MediaKind::Document | MediaKind::Location | MediaKind::LiveLocation => "m.file",
        };
        let mut content = json!({"msgtype": msgtype, "body": caption, "filename": name,
                                 "url": uri, "info": {"mimetype": mime_of_name(name)}});
        if kind == MediaKind::Voice {
            content["org.matrix.msc3245.voice"] = json!({});
        }
        Some(self.push(&content))
    }

    fn files(&self) -> Vec<MockFile> {
        self.lock().received.clone()
    }

    fn deleted(&self) -> Vec<String> {
        self.lock().deleted.clone()
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

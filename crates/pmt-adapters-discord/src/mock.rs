//! A Discord mock: the REST API (messages limited to 2000 characters,
//! edits, deletes, typing, reactions, channels and a thread, interaction
//! callbacks, CDN files, uploads and voice messages, scripted failures) and a Gateway WebSocket that numbers every event of
//! a session and replays the missed ones on Resume, as Discord does.

// A fixture: a mock that cannot serve should fail loudly.
#![allow(clippy::missing_panics_doc)]

use std::collections::VecDeque;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use futures_util::{SinkExt as _, StreamExt as _};
use http_body_util::{BodyExt as _, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use pmt_gateway::messaging::conformance::{
    Failure, MockFile, MockPlatform, MockSent, kind_of_name, mime_of_name,
};
use pmt_gateway::messaging::mock_server::form_parts;
use pmt_gateway::messaging::{Button, MediaKind};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

/// The bot token the mock accepts.
pub const TOKEN: &str = "discord-test-token";
/// The DM channel the kit talks in.
pub const DM: &str = "900";
/// The user the kit talks as.
pub const USER: &str = "4242";
/// The bot's user id.
pub const BOT: &str = "1";
/// A server text channel.
pub const CHANNEL: &str = "910";
/// A thread of [`CHANNEL`] (a channel of its own on Discord).
pub const THREAD: &str = "911";
/// The bot's application id.
pub const APPLICATION: &str = "777";
/// A forum channel of the server.
pub const FORUM: &str = "920";
/// The server.
pub const GUILD: &str = "800";
/// The `IS_VOICE_MESSAGE` flag.
const VOICE: u64 = 1 << 13;

/// What the Gateway socket task is told.
enum Push {
    Event(Value),
    Close(u16),
}

#[derive(Default)]
struct State {
    /// Every dispatch of the session, with its sequence number.
    events: Vec<(u64, Value)>,
    last_message: Option<Value>,
    next_id: u64,
    sent: Vec<MockSent>,
    typing: usize,
    reactions: Vec<(String, String)>,
    failures: VecDeque<Failure>,
    files: Vec<(String, Vec<u8>)>,
    uploads: usize,
    received: Vec<MockFile>,
    deleted: Vec<String>,
    /// The connected socket's mailbox.
    socket: Option<mpsc::UnboundedSender<Push>>,
    /// Identify close code to answer with (the intents scenario).
    refuse_identify: Option<u16>,
    identifies: usize,
    resumes: usize,
    /// Every `PUT /applications/{id}/commands` body.
    command_puts: Vec<Value>,
    /// Member roles by user (an unknown member fails the lookup).
    roles: std::collections::HashMap<String, Vec<String>>,
    /// Channel histories, oldest first.
    history: std::collections::HashMap<String, Vec<Value>>,
    /// Every interaction callback body.
    callbacks: Vec<Value>,
    /// Server channels and threads by id (anything else is a DM).
    channels: std::collections::HashMap<String, Value>,
}

/// The mock.
pub struct DiscordMock {
    state: Mutex<State>,
    rest: SocketAddr,
    gateway: SocketAddr,
}

impl DiscordMock {
    /// Starts the REST and Gateway servers on free loopback ports.
    pub async fn start() -> Arc<Self> {
        let rest = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let gateway = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mock = Arc::new(Self {
            state: Mutex::new(State {
                next_id: 1000,
                channels: [
                    (CHANNEL, json!({"id": CHANNEL, "type": 0, "guild_id": GUILD, "name": "ops"})),
                    (THREAD, json!({"id": THREAD, "type": 11, "guild_id": GUILD, "parent_id": CHANNEL, "name": "kit thread"})),
                    (FORUM, json!({"id": FORUM, "type": 15, "guild_id": GUILD, "name": "reports"})),
                ]
                .into_iter()
                .map(|(id, c)| (id.to_owned(), c))
                .collect(),
                ..State::default()
            }),
            rest: rest.local_addr().unwrap(),
            gateway: gateway.local_addr().unwrap(),
        });
        let server = Arc::clone(&mock);
        tokio::spawn(async move {
            while let Ok((stream, _)) = rest.accept().await {
                let server = Arc::clone(&server);
                tokio::spawn(async move {
                    let service = service_fn(move |req| {
                        let server = Arc::clone(&server);
                        async move { Ok::<_, Infallible>(server.rest(req).await) }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        let server = Arc::clone(&mock);
        tokio::spawn(async move {
            while let Ok((stream, _)) = gateway.accept().await {
                let server = Arc::clone(&server);
                tokio::spawn(async move { server.socket(stream).await });
            }
        });
        mock
    }

    /// The endpoints that reach this mock.
    pub fn endpoints(&self) -> crate::Endpoints {
        crate::Endpoints {
            api: format!("http://{}/api/v10", self.rest),
        }
    }

    /// Every application command overwrite received.
    pub fn command_puts(&self) -> Vec<Value> {
        self.lock().command_puts.clone()
    }

    /// Every interaction callback body received.
    pub fn callbacks(&self) -> Vec<Value> {
        self.lock().callbacks.clone()
    }

    /// The next Identify is refused with close code `code`.
    pub fn refuse_identify(&self, code: u16) {
        self.lock().refuse_identify = Some(code);
    }

    /// (Identify, Resume) counts.
    pub fn handshakes(&self) -> (usize, usize) {
        let state = self.lock();
        (state.identifies, state.resumes)
    }

    /// Stores a CDN file at `/cdn/<name>`; its URL.
    pub fn add_file(&self, name: &str, bytes: Vec<u8>) -> String {
        self.lock().files.push((name.to_owned(), bytes));
        format!("http://{}/cdn/{name}", self.rest)
    }

    /// Multipart uploads received.
    pub fn uploads(&self) -> usize {
        self.lock().uploads
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    fn id(&self) -> String {
        let mut state = self.lock();
        state.next_id += 1;
        state.next_id.to_string()
    }

    /// Records a dispatch and sends it when a socket is connected.
    pub fn dispatch(&self, kind: &str, data: &Value) {
        let mut state = self.lock();
        let seq = state.events.len() as u64 + 1;
        let event = json!({"op": 0, "t": kind, "s": seq, "d": data});
        state.events.push((seq, event.clone()));
        if let Some(socket) = &state.socket {
            let _ = socket.send(Push::Event(event));
        }
    }

    async fn socket(self: Arc<Self>, stream: tokio::net::TcpStream) {
        let Ok(socket) = tokio_tungstenite::accept_async(stream).await else {
            return;
        };
        let (mut sink, mut source) = socket.split();
        let hello = json!({"op": 10, "d": {"heartbeat_interval": 45_000}});
        let _ = sink.send(Message::text(hello.to_string())).await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        loop {
            tokio::select! {
                push = rx.recv() => match push {
                    Some(Push::Event(event)) => {
                        let _ = sink.send(Message::text(event.to_string())).await;
                    }
                    Some(Push::Close(code)) => {
                        let frame = CloseFrame { code: CloseCode::from(code), reason: "".into() };
                        let _ = sink.send(Message::Close(Some(frame))).await;
                        return;
                    }
                    None => return,
                },
                frame = source.next() => {
                    let Some(Ok(Message::Text(text))) = frame else { return };
                    let payload: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                    for reply in self.gateway_op(&payload, &tx) {
                        let _ = sink.send(reply).await;
                    }
                }
            }
        }
    }

    fn gateway_op(&self, payload: &Value, tx: &mpsc::UnboundedSender<Push>) -> Vec<Message> {
        let mut state = self.lock();
        match payload["op"].as_u64() {
            Some(1) => vec![Message::text(json!({"op": 11}).to_string())],
            Some(2) => {
                state.identifies += 1;
                if let Some(code) = state.refuse_identify.take() {
                    let _ = tx.send(Push::Close(code));
                    return Vec::new();
                }
                if payload["d"]["token"].as_str() != Some(TOKEN) {
                    let _ = tx.send(Push::Close(4004));
                    return Vec::new();
                }
                state.socket = Some(tx.clone());
                let ready = json!({"op": 0, "t": "READY", "s": state.events.len() as u64 + 1, "d": {
                    "session_id": "sess-1",
                    "resume_gateway_url": format!("ws://{}", self.gateway),
                    "user": {"id": BOT, "username": "kit-bot", "bot": true},
                    "application": {"id": APPLICATION}
                }});
                let seq = state.events.len() as u64 + 1;
                state.events.push((seq, ready.clone()));
                vec![Message::text(ready.to_string())]
            }
            Some(6) => {
                state.resumes += 1;
                state.socket = Some(tx.clone());
                let after = payload["d"]["seq"].as_u64().unwrap_or(0);
                let mut replies: Vec<Message> = state
                    .events
                    .iter()
                    .filter(|(seq, _)| *seq > after)
                    .map(|(_, e)| Message::text(e.to_string()))
                    .collect();
                replies.push(Message::text(
                    json!({"op": 0, "t": "RESUMED", "s": null, "d": {}}).to_string(),
                ));
                replies
            }
            _ => Vec::new(),
        }
    }

    async fn rest(self: Arc<Self>, request: Request<Incoming>) -> Response<Full<Bytes>> {
        let method = request.method().as_str().to_owned();
        let path = request.uri().path().to_owned();
        let authorized = request
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            == Some(&*format!("Bot {TOKEN}"));
        let kind = request
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let body = request
            .into_body()
            .collect()
            .await
            .map(|b| b.to_bytes().to_vec())
            .unwrap_or_default();
        if let Some(name) = path.strip_prefix("/cdn/") {
            let found = self
                .lock()
                .files
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, b)| b.clone());
            return match found {
                Some(bytes) => Response::new(Full::new(Bytes::from(bytes))),
                None => respond(404, &json!({"message": "Unknown"})),
            };
        }
        if !authorized {
            return respond(401, &json!({"message": "401: Unauthorized", "code": 0}));
        }
        let path = path.trim_start_matches("/api/v10").to_owned();
        let (status, answer) = self.route(&method, &path, &kind, &body);
        respond(status, &answer)
    }

    fn route(&self, method: &str, path: &str, kind: &str, body: &[u8]) -> (u16, Value) {
        let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        match (method, parts.as_slice()) {
            ("GET", ["users", "@me"]) => (200, json!({"id": BOT, "username": "kit-bot"})),
            ("GET", ["gateway", "bot"]) => (200, json!({"url": format!("ws://{}", self.gateway)})),
            ("POST", ["channels", channel, "messages"]) => self.create(channel, kind, body),
            ("PATCH", ["channels", _, "messages", id]) => self.edit(id, body),
            ("DELETE", ["channels", _, "messages", id]) => {
                self.lock().deleted.push((*id).to_owned());
                (204, Value::Null)
            }
            ("POST", ["channels", _, "typing"]) => {
                self.lock().typing += 1;
                (204, Value::Null)
            }
            ("PUT", ["channels", _, "messages", id, "reactions", emoji, "@me"]) => {
                let emoji = percent_encoding::percent_decode_str(emoji)
                    .decode_utf8_lossy()
                    .into_owned();
                self.lock().reactions.push(((*id).to_owned(), emoji));
                (204, Value::Null)
            }
            ("GET", ["channels", id]) => {
                let known = self.lock().channels.get(*id).cloned();
                (
                    200,
                    known.unwrap_or_else(|| {
                        json!({"id": id, "type": 1, "recipients": [{"id": USER, "username": "kit-user"}]})
                    }),
                )
            }
            ("PATCH", ["channels", id]) => {
                let update: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
                let mut state = self.lock();
                match state.channels.get_mut(*id) {
                    Some(channel) => {
                        channel["name"] = update["name"].clone();
                        (200, channel.clone())
                    }
                    None => (404, json!({"message": "Unknown Channel", "code": 10003})),
                }
            }
            ("POST", ["channels", channel, "messages", _, "threads"]) => {
                let args: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
                (
                    200,
                    self.open_thread(channel, args["name"].as_str().unwrap_or_default()),
                )
            }
            ("POST", ["channels", forum, "threads"]) => self.forum_post(forum, kind, body),
            ("POST", ["interactions", _, _, "callback"]) => {
                let body = serde_json::from_slice(body).unwrap_or(Value::Null);
                self.lock().callbacks.push(body);
                (204, Value::Null)
            }
            ("PUT", ["applications", APPLICATION, "commands"]) => {
                let body: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
                self.lock().command_puts.push(body.clone());
                (200, body)
            }
            ("GET", ["oauth2", "applications", "@me"]) => (200, json!({"id": APPLICATION})),
            ("GET", ["guilds", _, "members", user]) => match self.lock().roles.get(*user) {
                Some(roles) => (200, json!({"user": {"id": user}, "roles": roles})),
                None => (
                    500,
                    json!({"message": "500: Internal Server Error", "code": 0}),
                ),
            },
            ("GET", ["channels", channel, "messages"]) => {
                let history = self
                    .lock()
                    .history
                    .get(*channel)
                    .cloned()
                    .unwrap_or_default();
                // Newest first, as Discord lists them (the limit is the
                // adapter's to send; the mock answers everything).
                (200, Value::Array(history.into_iter().rev().collect()))
            }
            _ => (404, json!({"message": "404: Not Found", "code": 0})),
        }
    }

    fn open_thread(&self, parent: &str, name: &str) -> Value {
        let mut state = self.lock();
        state.next_id += 1;
        let id = state.next_id.to_string();
        let thread =
            json!({"id": id, "type": 11, "guild_id": GUILD, "parent_id": parent, "name": name});
        state.channels.insert(id, thread.clone());
        thread
    }

    /// A new forum post: a thread whose starter message is the payload's
    /// `message`.
    fn forum_post(&self, forum: &str, kind: &str, body: &[u8]) -> (u16, Value) {
        let parts = form_parts(kind, body);
        let args: Value = if kind.starts_with("multipart/") {
            parts
                .iter()
                .find(|p| p.name == "payload_json")
                .and_then(|p| serde_json::from_slice(&p.bytes).ok())
                .unwrap_or(Value::Null)
        } else {
            serde_json::from_slice(body).unwrap_or(Value::Null)
        };
        let mut thread = self.open_thread(forum, args["name"].as_str().unwrap_or_default());
        let id = thread["id"].as_str().unwrap_or_default().to_owned();
        let mut state = self.lock();
        for file in parts.iter().filter(|p| p.file_name.is_some()) {
            let name = file.file_name.clone().unwrap_or_default();
            state.received.push(MockFile {
                chat_id: forum.to_owned(),
                thread_id: Some(id.clone()),
                kind: kind_of_name(&name),
                name,
                bytes: file.bytes.clone(),
            });
        }
        state.sent.push(MockSent {
            chat_id: forum.to_owned(),
            message_id: id.clone(),
            text: args["message"]["content"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            buttons: Vec::new(),
            thread_id: Some(id.clone()),
        });
        thread["message"] = json!({"id": id});
        (201, thread)
    }

    /// Gives `user` the server roles `roles` (a member never given roles
    /// fails the lookup with a server error).
    pub fn set_roles(&self, user: &str, roles: &[&str]) {
        self.lock().roles.insert(
            user.to_owned(),
            roles.iter().map(|r| (*r).to_owned()).collect(),
        );
    }

    /// Appends a message by `author` (`BOT` for the bot) to a channel's
    /// history.
    pub fn post_history(&self, channel: &str, id: &str, author: &str, text: &str) {
        let message =
            json!({"id": id, "author": {"id": author, "username": author}, "content": text});
        self.lock()
            .history
            .entry(channel.to_owned())
            .or_default()
            .push(message);
    }

    /// Renames a channel, as a user would.
    pub fn rename_channel(&self, id: &str, name: &str) {
        if let Some(channel) = self.lock().channels.get_mut(id) {
            channel["name"] = json!(name);
        }
    }

    /// A channel's current name.
    pub fn channel_name(&self, id: &str) -> Option<String> {
        self.lock()
            .channels
            .get(id)
            .and_then(|c| c["name"].as_str().map(str::to_owned))
    }

    /// Threads opened on `parent`, as `(id, name)`.
    pub fn threads_of(&self, parent: &str) -> Vec<(String, String)> {
        let mut threads: Vec<(String, String)> = self
            .lock()
            .channels
            .values()
            .filter(|c| c["parent_id"] == parent && c["id"] != THREAD)
            .map(|c| {
                (
                    c["id"].as_str().unwrap_or_default().to_owned(),
                    c["name"].as_str().unwrap_or_default().to_owned(),
                )
            })
            .collect();
        threads.sort();
        threads
    }

    fn create(&self, channel: &str, kind: &str, body: &[u8]) -> (u16, Value) {
        let mut state = self.lock();
        if let Some(failure) = state.failures.pop_front() {
            return match failure {
                Failure::RateLimited(secs) => (
                    429,
                    json!({"message": "You are being rate limited.", "retry_after": secs, "global": false}),
                ),
                Failure::ServerError => (500, json!({"message": "500: Internal Server Error"})),
                Failure::Markup | Failure::Forbidden => (
                    403,
                    json!({"message": "Missing Permissions", "code": 50013}),
                ),
            };
        }
        // A thread is a channel of its own; the mock files it under its
        // parent.
        let parent = state
            .channels
            .get(channel)
            .filter(|c| c["type"] == 11)
            .and_then(|c| c["parent_id"].as_str().map(str::to_owned));
        let (chat, thread_id) = match parent {
            Some(parent) => (parent, Some(channel.to_owned())),
            None => (channel.to_owned(), None),
        };
        let chat = chat.as_str();
        let parts = form_parts(kind, body);
        let payload: Value = if kind.starts_with("multipart/") {
            state.uploads += 1;
            parts
                .iter()
                .find(|p| p.name == "payload_json")
                .and_then(|p| serde_json::from_slice(&p.bytes).ok())
                .unwrap_or(Value::Null)
        } else {
            serde_json::from_slice(body).unwrap_or(Value::Null)
        };
        let voice = payload["flags"].as_u64().unwrap_or(0) & VOICE != 0;
        for file in parts.iter().filter(|p| p.file_name.is_some()) {
            let name = file.file_name.clone().unwrap_or_default();
            state.received.push(MockFile {
                chat_id: chat.to_owned(),
                thread_id: thread_id.clone(),
                kind: if voice {
                    MediaKind::Voice
                } else {
                    kind_of_name(&name)
                },
                name,
                bytes: file.bytes.clone(),
            });
        }
        let content = payload["content"].as_str().unwrap_or_default();
        if content.chars().count() > 2000 {
            return (
                400,
                json!({"message": "Invalid Form Body", "code": 50035,
                       "errors": {"content": {"_errors": [{"code": "BASE_TYPE_MAX_LENGTH"}]}}}),
            );
        }
        state.next_id += 1;
        let id = state.next_id.to_string();
        state.sent.push(MockSent {
            chat_id: chat.to_owned(),
            message_id: id.clone(),
            text: content.to_owned(),
            buttons: buttons(&payload["components"]),
            thread_id,
        });
        (200, json!({"id": id, "channel_id": channel}))
    }

    fn edit(&self, id: &str, body: &[u8]) -> (u16, Value) {
        let payload: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
        let mut state = self.lock();
        let Some(message) = state.sent.iter_mut().find(|m| m.message_id == id) else {
            return (404, json!({"message": "Unknown Message", "code": 10008}));
        };
        payload["content"]
            .as_str()
            .unwrap_or_default()
            .clone_into(&mut message.text);
        message.buttons = buttons(&payload["components"]);
        (200, json!({"id": id}))
    }

    /// Delivers a DM with a file attached.
    pub fn inject_file(&self, url: &str, name: &str, content_type: &str) -> String {
        let id = self.id();
        self.dispatch(
            "MESSAGE_CREATE",
            &json!({
                "id": id, "channel_id": DM, "content": "",
                "author": {"id": USER, "username": "kit-user"},
                "timestamp": "2026-11-02T10:00:00+00:00",
                "attachments": [{"url": url, "filename": name, "content_type": content_type}]
            }),
        );
        id
    }
}

fn buttons(components: &Value) -> Vec<Button> {
    components
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|row| row["components"].as_array().cloned().unwrap_or_default())
        .map(|b| Button {
            label: b["label"].as_str().unwrap_or_default().to_owned(),
            data: b["custom_id"].as_str().unwrap_or_default().to_owned(),
        })
        .collect()
}

fn respond(status: u16, body: &Value) -> Response<Full<Bytes>> {
    let text = if body.is_null() {
        String::new()
    } else {
        body.to_string()
    };
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(text)))
        .unwrap()
}

impl MockPlatform for DiscordMock {
    fn dm(&self) -> (String, String) {
        (DM.to_owned(), USER.to_owned())
    }

    fn inject(&self, text: &str) -> String {
        let id = self.id();
        let message = json!({
            "id": id, "channel_id": DM, "content": text,
            "author": {"id": USER, "username": "kit-user"},
            "timestamp": "2026-11-02T10:00:00+00:00", "attachments": []
        });
        self.lock().last_message = Some(message.clone());
        self.dispatch("MESSAGE_CREATE", &message);
        id
    }

    fn redeliver(&self) {
        let last = self.lock().last_message.clone();
        if let Some(message) = last {
            self.dispatch("MESSAGE_CREATE", &message);
        }
    }

    fn drop_connection(&self) {
        // A resumable close: the client resumes and gets what it missed.
        if let Some(socket) = self.lock().socket.take() {
            let _ = socket.send(Push::Close(4000));
        }
    }

    fn press(&self, message_id: &str, data: &str) {
        let id = self.id();
        self.dispatch(
            "INTERACTION_CREATE",
            &json!({
                "id": id, "token": "itoken", "type": 3, "channel_id": DM,
                "user": {"id": USER, "username": "kit-user"},
                "data": {"custom_id": data, "component_type": 2},
                "message": {"id": message_id}
            }),
        );
    }

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
        Some((CHANNEL.to_owned(), THREAD.to_owned()))
    }

    fn inject_media(
        &self,
        kind: MediaKind,
        name: &str,
        bytes: &[u8],
        caption: &str,
    ) -> Option<String> {
        let url = self.add_file(name, bytes.to_vec());
        let id = self.id();
        let flags = if kind == MediaKind::Voice { VOICE } else { 0 };
        self.dispatch(
            "MESSAGE_CREATE",
            &json!({
                "id": id, "channel_id": DM, "content": caption, "flags": flags,
                "author": {"id": USER, "username": "kit-user"},
                "timestamp": "2026-11-02T10:00:00+00:00",
                "attachments": [{"url": url, "filename": name,
                                 "content_type": mime_of_name(name), "duration_secs": 1.0}]
            }),
        );
        Some(id)
    }

    fn files(&self) -> Vec<MockFile> {
        self.lock().received.clone()
    }

    fn deleted(&self) -> Vec<String> {
        self.lock().deleted.clone()
    }

    fn menu(&self) -> Option<Vec<String>> {
        let last = self.lock().command_puts.last().cloned();
        Some(
            last.and_then(|body| body.as_array().cloned())
                .unwrap_or_default()
                .iter()
                .filter_map(|c| c["name"].as_str().map(str::to_owned))
                .collect(),
        )
    }
}

/// The adapter, built for a fresh profile of the home at `home` that
/// holds the mock's token, and the mock it talks to: what
/// `prometheus plugins test-adapter discord` and the crate's tests run the
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
    let mock = DiscordMock::start().await;
    let setup = pmt_gateway::messaging::conformance::kit_setup(
        home,
        crate::PLATFORM,
        "",
        &[(crate::TOKEN_SECRET, TOKEN)],
    )?;
    let adapter = crate::Discord::new(&setup, mock.endpoints())?;
    Ok((Arc::new(adapter), mock))
}

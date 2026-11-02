//! A Slack mock: the Web API (messages up to 39,000 characters, threads,
//! Block Kit buttons, edits, deletes, reactions, conversation info, the
//! external file upload flow, private file downloads, scripted failures) and Socket Mode, which
//! keeps every envelope until it is acknowledged and delivers the
//! unacknowledged ones on the next connection, as Slack does.

// A fixture: a mock that cannot serve should fail loudly.
#![allow(clippy::missing_panics_doc)]

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use futures_util::{SinkExt as _, StreamExt as _};
use pmt_gateway::messaging::conformance::{
    Failure, MockFile, MockPlatform, MockSent, kind_of_name, mime_of_name,
};
use pmt_gateway::messaging::mock_server;
use pmt_gateway::messaging::{Button, HttpRequest, HttpResponse, MediaKind};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

/// The bot token the mock accepts.
pub const BOT_TOKEN: &str = "xoxb-kit";
/// The app-level token the mock accepts.
pub const APP_TOKEN: &str = "xapp-kit";
/// The DM conversation the kit talks in.
pub const DM: &str = "D0KIT";
/// The user the kit talks as.
pub const USER: &str = "U0KIT";
/// The bot's user id.
pub const BOT: &str = "U0BOT";
/// The workspace.
pub const TEAM: &str = "T0KIT";
/// A channel the kit posts threads in.
pub const CHANNEL: &str = "C0OPS";
/// A thread of [`CHANNEL`] (its root message's `ts`).
pub const THREAD: &str = "1700000000.000001";
const LIMIT: usize = 39_000;

#[derive(Default)]
struct State {
    /// Envelopes not yet acknowledged, in order.
    pending: Vec<(String, Value)>,
    last_event: Option<Value>,
    next: u64,
    sent: Vec<MockSent>,
    reactions: Vec<(String, String)>,
    failures: VecDeque<Failure>,
    files: Vec<(String, Vec<u8>)>,
    uploads: Vec<String>,
    /// File names by reserved upload id.
    reserved: Vec<(String, String)>,
    received: Vec<MockFile>,
    deleted: Vec<String>,
    socket: Option<mpsc::UnboundedSender<Option<Value>>>,
    connections: usize,
    /// Acknowledgements that carried a payload (slash command placeholders).
    ack_payloads: Vec<Value>,
    /// Bodies posted to slash commands' response URLs.
    responses: Vec<Value>,
}

/// The mock.
pub struct SlackMock {
    state: Mutex<State>,
    api: Mutex<Option<SocketAddr>>,
    socket: SocketAddr,
}

impl SlackMock {
    /// Starts the Web API and Socket Mode servers on free loopback ports.
    pub async fn start() -> Arc<Self> {
        let sockets = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mock = Arc::new(Self {
            state: Mutex::new(State {
                next: 1_700_000_000,
                ..State::default()
            }),
            api: Mutex::new(None),
            socket: sockets.local_addr().unwrap(),
        });
        let server = Arc::clone(&mock);
        let api = mock_server::serve(move |request| {
            let server = Arc::clone(&server);
            async move { server.web(&request) }
        })
        .await
        .unwrap();
        *mock.api.lock().unwrap() = Some(api);
        let server = Arc::clone(&mock);
        tokio::spawn(async move {
            while let Ok((stream, _)) = sockets.accept().await {
                let server = Arc::clone(&server);
                tokio::spawn(async move { server.socket_mode(stream).await });
            }
        });
        mock
    }

    /// The endpoints that reach this mock.
    pub fn endpoints(&self) -> crate::Endpoints {
        crate::Endpoints {
            api: format!("http://{}/api", self.api_addr()),
        }
    }

    fn api_addr(&self) -> SocketAddr {
        self.api.lock().unwrap().expect("started")
    }

    /// Stores a private file; its download URL.
    pub fn add_file(&self, name: &str, bytes: Vec<u8>) -> String {
        self.lock().files.push((name.to_owned(), bytes));
        format!("http://{}/files/{name}", self.api_addr())
    }

    /// Names of the files uploaded into conversations.
    pub fn uploads(&self) -> Vec<String> {
        self.lock().uploads.clone()
    }

    /// Socket Mode connections opened so far.
    pub fn connections(&self) -> usize {
        self.lock().connections
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    fn ts(&self) -> String {
        let mut state = self.lock();
        state.next += 1;
        format!("{}.000100", state.next)
    }

    /// Invokes slash command `command` with `text` in the kit's DM; the
    /// trigger id.
    pub fn slash(&self, command: &str, text: &str) -> String {
        let trigger = format!("trig-{}", self.ts());
        let url = format!("http://{}/response/{trigger}", self.api_addr());
        self.envelope(
            "slash_commands",
            &json!({"command": command, "text": text, "channel_id": DM,
                    "user_id": USER, "user_name": "kit", "trigger_id": trigger,
                    "response_url": url}),
        );
        trigger
    }

    /// Placeholders the adapter acknowledged slash commands with.
    pub fn ack_payloads(&self) -> Vec<Value> {
        self.lock().ack_payloads.clone()
    }

    /// Bodies posted to response URLs.
    pub fn responses(&self) -> Vec<Value> {
        self.lock().responses.clone()
    }

    /// Queues an envelope and pushes it to the connected socket.
    pub fn envelope(&self, kind: &str, payload: &Value) {
        let mut state = self.lock();
        state.next += 1;
        let id = format!("env-{}", state.next);
        let envelope = json!({"envelope_id": id, "type": kind, "payload": payload.clone(),
                              "retry_attempt": 0, "accepts_response_payload": false});
        state.pending.push((id, envelope.clone()));
        if let Some(socket) = &state.socket {
            let _ = socket.send(Some(envelope));
        }
    }

    /// Delivers a message event in the DM with the given fields added.
    pub fn inject_event(&self, mut event: Value) -> String {
        let ts = self.ts();
        event["type"] = json!("message");
        event["channel"] = json!(DM);
        event["channel_type"] = json!("im");
        event["user"] = json!(USER);
        event["ts"] = json!(ts);
        self.lock().last_event = Some(event.clone());
        self.envelope(
            "events_api",
            &json!({"team_id": TEAM, "event": event, "type": "event_callback"}),
        );
        ts
    }

    async fn socket_mode(self: Arc<Self>, stream: tokio::net::TcpStream) {
        let Ok(socket) = tokio_tungstenite::accept_async(stream).await else {
            return;
        };
        let (mut sink, mut source) = socket.split();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let backlog: Vec<Value> = {
            let mut state = self.lock();
            state.connections += 1;
            state.socket = Some(tx);
            state.pending.iter().map(|(_, e)| e.clone()).collect()
        };
        let hello = json!({"type": "hello", "num_connections": 1});
        let _ = sink.send(Message::text(hello.to_string())).await;
        for envelope in backlog {
            let _ = sink.send(Message::text(envelope.to_string())).await;
        }
        loop {
            tokio::select! {
                push = rx.recv() => {
                    let Some(Some(envelope)) = push else {
                        let _ = sink.send(Message::Close(None)).await;
                        return;
                    };
                    let _ = sink.send(Message::text(envelope.to_string())).await;
                }
                frame = source.next() => {
                    let Some(Ok(Message::Text(text))) = frame else { return };
                    let ack: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                    if let Some(id) = ack["envelope_id"].as_str() {
                        let mut state = self.lock();
                        state.pending.retain(|(e, _)| e != id);
                        if ack["payload"].is_object() {
                            state.ack_payloads.push(ack["payload"].clone());
                        }
                    }
                }
            }
        }
    }

    fn web(&self, request: &HttpRequest) -> HttpResponse {
        if let Some(name) = request.path.strip_prefix("/files/") {
            let found = self.lock().files.iter().find(|(n, _)| n == name).cloned();
            return match (found, request.header("authorization")) {
                (Some((_, bytes)), Some(auth)) if auth == format!("Bearer {BOT_TOKEN}") => {
                    mock_server::bytes(200, bytes)
                }
                _ => HttpResponse::status(404),
            };
        }
        if request.path.starts_with("/response/") {
            let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
            self.lock().responses.push(body);
            return HttpResponse::status(200);
        }
        if let Some(id) = request.path.strip_prefix("/upload/") {
            let mut state = self.lock();
            state
                .files
                .push((format!("upload-{id}"), request.body.clone()));
            return HttpResponse::status(200);
        }
        let method = request.path.trim_start_matches("/api/");
        let token = request
            .header("authorization")
            .and_then(|a| a.strip_prefix("Bearer "))
            .unwrap_or_default();
        let wanted = if method == "apps.connections.open" {
            APP_TOKEN
        } else {
            BOT_TOKEN
        };
        if token != wanted {
            return refused("invalid_auth");
        }
        let args: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        self.method(method, &args)
    }

    fn method(&self, method: &str, args: &Value) -> HttpResponse {
        match method {
            "auth.test" => ok(&json!({"user_id": BOT, "team_id": TEAM, "user": "kit-bot"})),
            "apps.connections.open" => ok(&json!({"url": format!("ws://{}/link", self.socket)})),
            "chat.postMessage" => self.post(args),
            "chat.update" => self.update(args),
            "reactions.add" => {
                let entry = (
                    args["timestamp"].as_str().unwrap_or_default().to_owned(),
                    args["name"].as_str().unwrap_or_default().to_owned(),
                );
                self.lock().reactions.push(entry);
                ok(&json!({}))
            }
            "conversations.info" => {
                let id = args["channel"].as_str().unwrap_or_default();
                ok(&json!({"channel": {"id": id, "is_im": id.starts_with('D'), "user": USER}}))
            }
            "conversations.list" => ok(&json!({"channels": [{"id": "C0OPS", "name": "ops"}],
                                              "response_metadata": {"next_cursor": ""}})),
            "files.getUploadURLExternal" => {
                let id = self.ts();
                let name = args["filename"].as_str().unwrap_or_default().to_owned();
                self.lock().reserved.push((format!("F{id}"), name));
                ok(
                    &json!({"upload_url": format!("http://{}/upload/{id}", self.api_addr()),
                           "file_id": format!("F{id}")}),
                )
            }
            "files.completeUploadExternal" => self.complete(args),
            "chat.delete" => {
                let ts = args["ts"].as_str().unwrap_or_default().to_owned();
                self.lock().deleted.push(ts);
                ok(&json!({}))
            }
            _ => refused("unknown_method"),
        }
    }

    /// Files an upload into its conversation (and thread).
    fn complete(&self, args: &Value) -> HttpResponse {
        let id = args["files"][0]["id"].as_str().unwrap_or_default();
        let title = args["files"][0]["title"].as_str().unwrap_or_default();
        let mut state = self.lock();
        state.uploads.push(title.to_owned());
        let name = state
            .reserved
            .iter()
            .find(|(f, _)| f == id)
            .map(|(_, n)| n.clone());
        let upload = format!("upload-{}", id.trim_start_matches('F'));
        let bytes = state
            .files
            .iter()
            .find(|(n, _)| *n == upload)
            .map(|(_, b)| b.clone());
        let (Some(name), Some(bytes)) = (name, bytes) else {
            return refused("file_not_found");
        };
        state.received.push(MockFile {
            chat_id: args["channel_id"].as_str().unwrap_or_default().to_owned(),
            thread_id: args["thread_ts"].as_str().map(str::to_owned),
            kind: kind_of_name(&name),
            name,
            bytes,
        });
        ok(&json!({"files": []}))
    }

    fn post(&self, args: &Value) -> HttpResponse {
        if let Some(failure) = self.lock().failures.pop_front() {
            return match failure {
                Failure::RateLimited(secs) => {
                    HttpResponse::json(429, &json!({"ok": false, "error": "ratelimited"}))
                        .with_header("retry-after", &secs.to_string())
                }
                Failure::ServerError => HttpResponse::status(503),
                Failure::Markup => refused("invalid_blocks"),
                Failure::Forbidden => refused("not_in_channel"),
            };
        }
        let text = args["text"].as_str().unwrap_or_default();
        if text.chars().count() > LIMIT {
            return refused("msg_too_long");
        }
        let ts = self.ts();
        let channel = args["channel"].as_str().unwrap_or_default().to_owned();
        self.lock().sent.push(MockSent {
            chat_id: channel.clone(),
            message_id: ts.clone(),
            text: text.to_owned(),
            buttons: buttons(&args["blocks"]),
            thread_id: args["thread_ts"].as_str().map(str::to_owned),
        });
        ok(&json!({"channel": channel, "ts": ts}))
    }

    fn update(&self, args: &Value) -> HttpResponse {
        let ts = args["ts"].as_str().unwrap_or_default();
        let mut state = self.lock();
        let Some(message) = state.sent.iter_mut().find(|m| m.message_id == ts) else {
            return refused("message_not_found");
        };
        args["text"]
            .as_str()
            .unwrap_or_default()
            .clone_into(&mut message.text);
        message.buttons = buttons(&args["blocks"]);
        ok(&json!({"ts": ts}))
    }
}

fn ok(body: &Value) -> HttpResponse {
    let mut body = body.clone();
    body["ok"] = json!(true);
    HttpResponse::json(200, &body)
}

fn refused(error: &str) -> HttpResponse {
    HttpResponse::json(200, &json!({"ok": false, "error": error}))
}

fn buttons(blocks: &Value) -> Vec<Button> {
    blocks
        .as_array()
        .into_iter()
        .flatten()
        .filter(|b| b["type"] == "actions")
        .flat_map(|b| b["elements"].as_array().cloned().unwrap_or_default())
        .map(|e| Button {
            label: e["text"]["text"].as_str().unwrap_or_default().to_owned(),
            data: e["value"].as_str().unwrap_or_default().to_owned(),
        })
        .collect()
}

impl MockPlatform for SlackMock {
    fn dm(&self) -> (String, String) {
        (DM.to_owned(), USER.to_owned())
    }

    fn inject(&self, text: &str) -> String {
        self.inject_event(json!({"text": text}))
    }

    fn redeliver(&self) {
        let last = self.lock().last_event.clone();
        if let Some(event) = last {
            self.envelope(
                "events_api",
                &json!({"team_id": TEAM, "event": event, "type": "event_callback"}),
            );
        }
    }

    fn drop_connection(&self) {
        if let Some(socket) = self.lock().socket.take() {
            let _ = socket.send(None);
        }
    }

    fn press(&self, message_id: &str, data: &str) {
        let trigger = self.ts();
        self.envelope(
            "interactive",
            &json!({
                "type": "block_actions", "trigger_id": trigger,
                "user": {"id": USER, "username": "kit-user"},
                "team": {"id": TEAM},
                "channel": {"id": DM},
                "container": {"type": "message", "message_ts": message_id, "channel_id": DM},
                "message": {"ts": message_id},
                "actions": [{"action_id": "pmt_0_0", "value": data, "action_ts": trigger}]
            }),
        );
    }

    fn fail_next(&self, failure: Failure) {
        self.lock().failures.push_back(failure);
    }

    fn reports_markup_errors(&self) -> bool {
        true
    }

    fn sent(&self) -> Vec<MockSent> {
        self.lock().sent.clone()
    }

    fn typing(&self) -> usize {
        0
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
        // A recorded clip is Slack's voice note.
        let subtype = if kind == MediaKind::Voice {
            "slack_audio"
        } else {
            ""
        };
        Some(self.inject_event(json!({
            "text": caption,
            "files": [{"name": name, "mimetype": mime_of_name(name), "subtype": subtype,
                       "url_private_download": url}]
        })))
    }

    fn files(&self) -> Vec<MockFile> {
        self.lock().received.clone()
    }

    fn voice_notes_out(&self) -> bool {
        false
    }

    fn deleted(&self) -> Vec<String> {
        self.lock().deleted.clone()
    }
}

/// The adapter, built for a fresh profile of the home at `home` that
/// holds the mock's tokens, and the mock it talks to: what
/// `prometheus plugins test-adapter slack` and the crate's tests run the
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
    let mock = SlackMock::start().await;
    let setup = pmt_gateway::messaging::conformance::kit_setup(
        home,
        crate::PLATFORM,
        "",
        &[
            (crate::TOKEN_SECRET, BOT_TOKEN),
            (crate::APP_TOKEN_SECRET, APP_TOKEN),
        ],
    )?;
    let adapter = crate::Slack::new(&setup, mock.endpoints())?;
    Ok((Arc::new(adapter), mock))
}

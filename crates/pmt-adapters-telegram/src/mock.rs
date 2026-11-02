//! A Bot API mock server: long polling over a server-side update queue
//! confirmed by `offset` (as Telegram keeps unconfirmed updates), message
//! sends limited to 4096 UTF-16 code units, edits, deletes, typing,
//! reactions, button presses, topics (threads), files both ways and
//! scripted failures.

// A fixture: a mock that cannot serve should fail loudly.
#![allow(clippy::missing_panics_doc)]

use std::collections::VecDeque;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt as _, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use pmt_gateway::messaging::conformance::{Failure, MockFile, MockPlatform, MockSent};
use pmt_gateway::messaging::mock_server::{FormPart, form_parts};
use pmt_gateway::messaging::{Button, MediaKind};
use serde_json::{Value, json};
use tokio::sync::Notify;

/// The bot token the mock accepts.
pub const TOKEN: &str = "123:TEST";
/// The DM chat (and user) the kit talks in.
pub const DM: i64 = 4242;
/// The DM topic (thread) the kit posts in.
pub const TOPIC: &str = "77";
/// The longest message, in UTF-16 code units.
const LIMIT: usize = 4096;

#[derive(Default)]
struct State {
    updates: Vec<(i64, Value)>,
    next_update: i64,
    next_message: i64,
    last_update: Option<Value>,
    sent: Vec<MockSent>,
    typing: usize,
    reactions: Vec<(String, String)>,
    failures: VecDeque<Failure>,
    drop_next_poll: bool,
    files: Vec<(String, Vec<u8>)>,
    uploads: Vec<String>,
    received: Vec<MockFile>,
    deleted: Vec<String>,
    /// Every call of a method without its own handling, in order.
    calls: Vec<(String, Value)>,
    /// `setWebhook`'s URL and secret token while a webhook is set.
    webhook: Option<(String, String)>,
    /// Chats where `createForumTopic` fails ("the chat is not a forum").
    not_forums: Vec<String>,
    next_thread: i64,
}

/// The mock server.
pub struct BotApi {
    state: Mutex<State>,
    arrived: Notify,
    addr: SocketAddr,
}

fn ok(result: &Value) -> (u16, Value) {
    (200, json!({"ok": true, "result": result}))
}

fn refused(code: u16, description: &str) -> (u16, Value) {
    (
        code,
        json!({"ok": false, "error_code": code, "description": description}),
    )
}

impl BotApi {
    /// Starts a mock on a free loopback port.
    pub async fn start() -> Arc<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let api = Arc::new(Self {
            state: Mutex::new(State {
                next_update: 1,
                next_message: 100,
                next_thread: 500,
                ..State::default()
            }),
            arrived: Notify::new(),
            addr,
        });
        let server = Arc::clone(&api);
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let server = Arc::clone(&server);
                tokio::spawn(async move {
                    let service = service_fn(move |req| {
                        let server = Arc::clone(&server);
                        async move { Ok::<_, Infallible>(server.serve(req).await) }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        api
    }

    /// The endpoints that reach this mock.
    pub fn endpoints(&self) -> crate::Endpoints {
        crate::Endpoints {
            api: format!("http://{}", self.addr),
            files: format!("http://{}/file", self.addr),
        }
    }

    /// Stores a file the bot can download by `file_id`.
    pub fn add_file(&self, file_id: &str, bytes: Vec<u8>) {
        self.lock().files.push((file_id.to_owned(), bytes));
    }

    /// Methods of the uploads received (`sendPhoto`, ...).
    pub fn uploads(&self) -> Vec<String> {
        self.lock().uploads.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    /// Queues an update; while a webhook is set it is posted there
    /// instead, with the secret token, as Telegram does.
    pub fn push(&self, update: Value) {
        let mut state = self.lock();
        let id = state.next_update;
        state.next_update += 1;
        let mut update = update;
        update["update_id"] = json!(id);
        state.last_update = Some(update.clone());
        if let Some((url, secret)) = state.webhook.clone() {
            drop(state);
            tokio::spawn(async move {
                let client = pmt_net::http::client_builder(&pmt_net::OutboundPolicy::default())
                    .unwrap()
                    .no_proxy()
                    .build()
                    .unwrap();
                let _ = client
                    .post(url)
                    .header("x-telegram-bot-api-secret-token", secret)
                    .header("content-type", "application/json")
                    .body(update.to_string())
                    .send()
                    .await;
            });
            return;
        }
        state.updates.push((id, update));
        drop(state);
        self.arrived.notify_waiters();
    }

    /// The arguments of every call of `method` (methods the mock does not
    /// otherwise model), in order.
    pub fn calls(&self, method: &str) -> Vec<Value> {
        self.lock()
            .calls
            .iter()
            .filter(|(m, _)| m == method)
            .map(|(_, args)| args.clone())
            .collect()
    }

    /// Every method called, in order (getUpdates included).
    pub fn methods(&self) -> Vec<String> {
        self.lock().calls.iter().map(|(m, _)| m.clone()).collect()
    }

    /// `createForumTopic` fails in `chat` from now on.
    pub fn not_a_forum(&self, chat: &str) {
        self.lock().not_forums.push(chat.to_owned());
    }

    /// The webhook URL and secret token, while one is set.
    pub fn webhook(&self) -> Option<(String, String)> {
        self.lock().webhook.clone()
    }

    async fn serve(self: Arc<Self>, request: Request<Incoming>) -> Response<Full<Bytes>> {
        let path = request.uri().path().to_owned();
        let form = request
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
        if let Some(file) = path.strip_prefix(&format!("/file/bot{TOKEN}/files/")) {
            let found = self
                .lock()
                .files
                .iter()
                .find(|(id, _)| id == file)
                .map(|(_, bytes)| bytes.clone());
            return match found {
                Some(bytes) => Response::new(Full::new(Bytes::from(bytes))),
                None => respond(refused(404, "Not Found")),
            };
        }
        let Some(rest) = path.strip_prefix("/bot") else {
            return respond(refused(404, "Not Found"));
        };
        let (token, method) = rest.split_once('/').unwrap_or((rest, ""));
        if token != TOKEN {
            return respond(refused(401, "Unauthorized"));
        }
        let args: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        self.lock().calls.push((method.to_owned(), args.clone()));
        let answer = match method {
            "getUpdates" => self.get_updates(&args).await,
            method if method.starts_with("send") && method != "sendChatAction" => {
                self.send(method, &args, &form_parts(&form, &body))
            }
            other => self.other(other, &args),
        };
        respond(answer)
    }

    async fn get_updates(&self, args: &Value) -> (u16, Value) {
        let offset = args["offset"].as_i64().unwrap_or(0);
        let wait = Duration::from_secs(args["timeout"].as_u64().unwrap_or(0));
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let notified = self.arrived.notified();
            {
                let mut state = self.lock();
                if std::mem::take(&mut state.drop_next_poll) {
                    return refused(502, "Bad Gateway");
                }
                // Confirmed updates are forgotten, as Telegram does.
                state.updates.retain(|(id, _)| *id >= offset);
                if !state.updates.is_empty() {
                    let batch: Vec<Value> = state.updates.iter().map(|(_, u)| u.clone()).collect();
                    return ok(&json!(batch));
                }
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return ok(&json!([]));
            }
        }
    }

    fn send(&self, method: &str, args: &Value, form: &[FormPart]) -> (u16, Value) {
        let mut state = self.lock();
        if let Some(failure) = state.failures.pop_front() {
            return match failure {
                Failure::RateLimited(secs) => (
                    429,
                    json!({"ok": false, "error_code": 429, "description": "Too Many Requests",
                           "parameters": {"retry_after": secs}}),
                ),
                Failure::ServerError => refused(500, "Internal Server Error"),
                Failure::Markup => refused(400, "Bad Request: can't parse entities"),
                Failure::Forbidden => refused(403, "Forbidden: bot was blocked by the user"),
            };
        }
        let id = state.next_message;
        state.next_message += 1;
        if method != "sendMessage" {
            // A multipart upload: the chat and thread are form fields.
            state.uploads.push(method.to_owned());
            let field = |name: &str| form.iter().find(|p| p.name == name).map(FormPart::text);
            let chat = field("chat_id").unwrap_or_default();
            let thread_id = field("message_thread_id");
            if let Some(file) = form.iter().find(|p| p.file_name.is_some()) {
                state.received.push(MockFile {
                    chat_id: chat.clone(),
                    thread_id: thread_id.clone(),
                    name: file.file_name.clone().unwrap_or_default(),
                    kind: upload_kind(method),
                    bytes: file.bytes.clone(),
                });
            }
            state.sent.push(MockSent {
                chat_id: chat,
                message_id: id.to_string(),
                text: String::new(),
                buttons: Vec::new(),
                thread_id,
            });
            return ok(&json!({"message_id": id}));
        }
        let text = args["text"].as_str().unwrap_or_default();
        if text.encode_utf16().count() > LIMIT {
            return refused(400, "Bad Request: message is too long");
        }
        let buttons = buttons(&args["reply_markup"]);
        state.sent.push(MockSent {
            chat_id: args["chat_id"].to_string().trim_matches('"').to_owned(),
            message_id: id.to_string(),
            text: text.to_owned(),
            buttons,
            thread_id: normalize_id(&args["message_thread_id"]),
        });
        ok(&json!({"message_id": id, "chat": {"id": args["chat_id"]}}))
    }

    fn other(&self, method: &str, args: &Value) -> (u16, Value) {
        let mut state = self.lock();
        match method {
            "getMe" => ok(&json!({"id": 1, "is_bot": true, "username": "kit_bot"})),
            "editMessageText" => {
                let id = args["message_id"].to_string();
                let Some(message) = state.sent.iter_mut().find(|m| m.message_id == id) else {
                    return refused(400, "Bad Request: message to edit not found");
                };
                args["text"]
                    .as_str()
                    .unwrap_or_default()
                    .clone_into(&mut message.text);
                message.buttons = buttons(&args["reply_markup"]);
                ok(&json!(true))
            }
            "deleteMessage" => {
                let id = args["message_id"].to_string();
                state.deleted.push(id);
                ok(&json!(true))
            }
            "sendChatAction" => {
                state.typing += 1;
                ok(&json!(true))
            }
            "setMessageReaction" => {
                let emoji = args["reaction"][0]["emoji"].as_str().unwrap_or_default();
                state
                    .reactions
                    .push((args["message_id"].to_string(), emoji.to_owned()));
                ok(&json!(true))
            }
            "getChat" => {
                ok(&json!({"id": args["chat_id"], "type": "private", "first_name": "Kit"}))
            }
            "answerCallbackQuery" | "setMyCommands" | "deleteMyCommands" | "answerInlineQuery" => {
                ok(&json!(true))
            }
            "setWebhook" => {
                let url = args["url"].as_str().unwrap_or_default().to_owned();
                let secret = args["secret_token"].as_str().unwrap_or_default().to_owned();
                state.webhook = Some((url, secret));
                ok(&json!(true))
            }
            "deleteWebhook" => {
                state.webhook = None;
                ok(&json!(true))
            }
            "createForumTopic" => {
                let chat = args["chat_id"].to_string().trim_matches('"').to_owned();
                if state.not_forums.contains(&chat) {
                    return refused(400, "Bad Request: the chat is not a forum");
                }
                state.next_thread += 1;
                let thread = state.next_thread;
                ok(&json!({"message_thread_id": thread, "name": args["name"]}))
            }
            "getFile" => {
                let id = args["file_id"].as_str().unwrap_or_default();
                let size = state
                    .files
                    .iter()
                    .find(|(f, _)| f == id)
                    .map(|(_, b)| b.len());
                match size {
                    Some(size) => ok(
                        &json!({"file_id": id, "file_path": format!("files/{id}"), "file_size": size}),
                    ),
                    None => refused(400, "Bad Request: invalid file_id"),
                }
            }
            _ => refused(404, "Not Found: method not found"),
        }
    }

    /// Delivers a message with a photo whose file the bot downloads.
    pub fn inject_photo(&self, file_id: &str, caption: &str) {
        let id = self.next_message();
        self.push(json!({"message": {
            "message_id": id, "date": 1_700_000_000,
            "chat": {"id": DM, "type": "private"},
            "from": {"id": DM, "first_name": "Kit", "is_bot": false},
            "caption": caption, "photo": [{"file_id": file_id}]
        }}));
    }

    fn next_message(&self) -> i64 {
        let mut state = self.lock();
        let id = state.next_message;
        state.next_message += 1;
        id
    }
}

/// What the chat shows an upload as, by its method.
fn upload_kind(method: &str) -> MediaKind {
    match method {
        "sendPhoto" => MediaKind::Image,
        "sendAudio" => MediaKind::Audio,
        "sendVoice" => MediaKind::Voice,
        "sendVideo" => MediaKind::Video,
        _ => MediaKind::Document,
    }
}

/// The message key a file of `kind` arrives under.
fn message_key(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Image => "photo",
        MediaKind::Audio => "audio",
        MediaKind::Voice => "voice",
        MediaKind::Video => "video",
        MediaKind::Document => "document",
    }
}

fn normalize_id(value: &Value) -> Option<String> {
    match value {
        Value::Number(n) => Some(n.to_string()),
        Value::String(s) => Some(s.clone()),
        _ => None,
    }
}

fn buttons(markup: &Value) -> Vec<Button> {
    markup["inline_keyboard"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|row| row.as_array().cloned().unwrap_or_default())
        .map(|b| Button {
            label: b["text"].as_str().unwrap_or_default().to_owned(),
            data: b["callback_data"].as_str().unwrap_or_default().to_owned(),
        })
        .collect()
}

fn respond((status, body): (u16, Value)) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap()
}

impl MockPlatform for BotApi {
    fn dm(&self) -> (String, String) {
        (DM.to_string(), DM.to_string())
    }

    fn inject(&self, text: &str) -> String {
        let id = self.next_message();
        self.push(json!({"message": {
            "message_id": id, "date": 1_700_000_000,
            "chat": {"id": DM, "type": "private"},
            "from": {"id": DM, "first_name": "Kit", "is_bot": false},
            "text": text
        }}));
        id.to_string()
    }

    fn redeliver(&self) {
        let last = self.lock().last_update.clone();
        if let Some(update) = last {
            self.push(update);
        }
    }

    fn drop_connection(&self) {
        self.lock().drop_next_poll = true;
        self.arrived.notify_waiters();
    }

    fn press(&self, message_id: &str, data: &str) {
        let query = format!("q{message_id}");
        self.push(json!({"callback_query": {
            "id": query, "data": data,
            "from": {"id": DM, "first_name": "Kit"},
            "message": {"message_id": message_id.parse::<i64>().unwrap_or(0),
                        "chat": {"id": DM, "type": "private"}}
        }}));
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
        self.lock().typing
    }

    fn reactions(&self) -> Vec<(String, String)> {
        self.lock().reactions.clone()
    }

    fn thread(&self) -> Option<(String, String)> {
        Some((DM.to_string(), TOPIC.to_owned()))
    }

    fn inject_media(
        &self,
        kind: MediaKind,
        name: &str,
        bytes: &[u8],
        caption: &str,
    ) -> Option<String> {
        let file_id = format!("kit-{name}");
        self.add_file(&file_id, bytes.to_vec());
        let file = json!({"file_id": file_id, "file_name": name, "duration": 1});
        let id = self.next_message();
        let mut message = json!({
            "message_id": id, "date": 1_700_000_000,
            "chat": {"id": DM, "type": "private"},
            "from": {"id": DM, "first_name": "Kit", "is_bot": false},
            "caption": caption
        });
        message[message_key(kind)] = if kind == MediaKind::Image {
            json!([file])
        } else {
            file
        };
        self.push(json!({"message": message}));
        Some(id.to_string())
    }

    fn files(&self) -> Vec<MockFile> {
        self.lock().received.clone()
    }

    fn deleted(&self) -> Vec<String> {
        self.lock().deleted.clone()
    }

    fn menu(&self) -> Option<Vec<String>> {
        let last = self.calls("setMyCommands").pop();
        Some(
            last.and_then(|args| args["commands"].as_array().cloned())
                .unwrap_or_default()
                .iter()
                .filter_map(|c| c["command"].as_str().map(str::to_owned))
                .collect(),
        )
    }
}

/// The adapter, built for a fresh profile of the home at `home` that
/// holds the mock's token, and the mock it talks to: what
/// `prometheus plugins test-adapter telegram` and the crate's tests run the
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
    let mock = BotApi::start().await;
    let setup = pmt_gateway::messaging::conformance::kit_setup(
        home,
        crate::PLATFORM,
        "options = { poll_timeout_secs = 1 }",
        &[(crate::TOKEN_SECRET, TOKEN)],
    )?;
    let adapter = crate::Telegram::new(&setup, mock.endpoints())?;
    Ok((Arc::new(adapter), mock))
}

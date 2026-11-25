//! A mock Feishu for the `feishu` catalog adapter tests: the Open API
//! (tenant token, bot info, messages, uploads, resources, reactions,
//! chats, drive comments, documents) and the long connection (the
//! `pbbp2` binary frames the bridge sidecar carries) on one loopback
//! port.

#![allow(dead_code)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use pmt_gateway::messaging::Button;
use pmt_gateway::messaging::conformance::{
    Failure, MockFile, MockPlatform, MockSent, kind_of_name,
};
use pmt_gateway::messaging::descriptor::MediaKind;
use serde_json::{Value, json};

use super::catalog::{self, Bridge, Reply, Request};
use super::catalog_ws::{Socket, WsServer};

pub const APP_ID: &str = "cli_test";
pub const SECRET: &str = "app-secret";
pub const TOKEN: &str = "t-tenant";
pub const BOT: &str = "ou_bot";
pub const DM: &str = "oc_dm";
pub const GROUP: &str = "oc_group";
pub const USER: &str = "ou_user";

/// What the mock saw and holds.
#[derive(Default)]
pub struct State {
    queued: Vec<Vec<u8>>,
    last: Option<Vec<u8>>,
    pub sent: Vec<MockSent>,
    /// (message id, message type) of everything sent.
    pub kinds: Vec<(String, String)>,
    /// (replied-to id, reply id).
    pub replies: Vec<(String, String)>,
    pub files: Vec<MockFile>,
    uploads: Vec<(String, String, Vec<u8>)>,
    resources: Vec<(String, Vec<u8>)>,
    /// Chat of each message the mock delivered.
    origins: Vec<(String, String)>,
    pub deleted: Vec<String>,
    pub reactions: Vec<(String, String)>,
    /// `message_id` headers of acknowledged data frames.
    pub acks: Vec<String>,
    pub fail_next: Option<Failure>,
    /// Refuse `post` messages with a content error.
    pub refuse_posts: bool,
    /// Drive and document calls: (method, path with query, body).
    pub drive: Vec<(String, String, Value)>,
    /// The replies of the comment thread the drive API serves.
    pub thread: Vec<Value>,
    /// Whether that comment is a whole-document one.
    pub whole: bool,
}

/// The mock.
pub struct Feishu {
    pub server: WsServer,
    pub state: Arc<Mutex<State>>,
    next: AtomicU64,
}

fn varint(mut value: u64, out: &mut Vec<u8>) {
    loop {
        let bits = u8::try_from(value & 0x7f).unwrap();
        value >>= 7;
        if value == 0 {
            out.push(bits);
            return;
        }
        out.push(bits | 0x80);
    }
}

fn bytes_field(n: u64, value: &[u8], out: &mut Vec<u8>) {
    varint((n << 3) | 2, out);
    varint(value.len() as u64, out);
    out.extend_from_slice(value);
}

/// A data frame carrying `payload`.
pub fn data_frame(message_id: &str, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for (n, v) in [(1u64, 1u64), (2, 1), (3, 7), (4, 1)] {
        varint(n << 3, &mut out);
        varint(v, &mut out);
    }
    for (k, v) in [
        ("type", "event"),
        ("message_id", message_id),
        ("sum", "1"),
        ("seq", "0"),
    ] {
        let mut header = Vec::new();
        bytes_field(1, k.as_bytes(), &mut header);
        bytes_field(2, v.as_bytes(), &mut header);
        bytes_field(5, &header, &mut out);
    }
    bytes_field(8, payload, &mut out);
    out
}

fn read_varint(data: &[u8], at: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *data.get(*at)?;
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

/// One `Header {key = 1, value = 2}` message.
fn header(field: &[u8]) -> Option<(String, String)> {
    let (mut i, mut kv) = (0, Vec::new());
    while i < field.len() {
        read_varint(field, &mut i)?;
        let l = usize::try_from(read_varint(field, &mut i)?).ok()?;
        kv.push(String::from_utf8_lossy(field.get(i..i + l)?).into_owned());
        i += l;
    }
    match kv.as_slice() {
        [k, v] => Some((k.clone(), v.clone())),
        _ => None,
    }
}

/// The method and headers of a frame the adapter sent.
fn frame_headers(bytes: &[u8]) -> (u64, Vec<(String, String)>) {
    let (mut method, mut headers, mut at) = (0, Vec::new(), 0);
    while let Some(tag) = read_varint(bytes, &mut at) {
        let value = read_varint(bytes, &mut at).unwrap_or(0);
        if tag & 7 != 2 {
            if tag >> 3 == 4 {
                method = value;
            }
            continue;
        }
        let len = usize::try_from(value).unwrap();
        let field = &bytes[at..at + len];
        at += len;
        if tag >> 3 == 5 {
            headers.extend(header(field));
        }
    }
    (method, headers)
}

/// An event envelope (schema 2.0).
pub fn envelope(event_id: &str, event_type: &str, event: &Value) -> Value {
    json!({"schema": "2.0",
        "header": {"event_id": event_id, "event_type": event_type, "token": "verify-token", "app_id": APP_ID},
        "event": event})
}

/// An `im.message.receive_v1` event.
pub fn message_event(
    id: &str,
    chat: &str,
    sender: &str,
    kind: &str,
    content: &Value,
    mentions: &Value,
) -> Value {
    let chat_type = if chat == DM { "p2p" } else { "group" };
    envelope(
        &format!("ev-{id}"),
        "im.message.receive_v1",
        &json!({
            "sender": {"sender_id": {"open_id": sender}, "sender_type": "user"},
            "message": {"message_id": id, "chat_id": chat, "chat_type": chat_type,
                "message_type": kind, "content": content.to_string(), "mentions": mentions},
        }),
    )
}

/// The bot's mention, for `message_event`'s `mentions`.
pub fn bot_mention() -> Value {
    json!([{"key": "@_user_1", "id": {"open_id": BOT}, "name": "Bot"}])
}

/// What a message shows: its text and buttons.
fn shown(kind: &str, content: &str) -> (String, Vec<Button>) {
    let value: Value = serde_json::from_str(content).unwrap_or(Value::Null);
    match kind {
        "text" => (
            value["text"].as_str().unwrap_or_default().to_owned(),
            Vec::new(),
        ),
        "post" => {
            let rows = value["zh_cn"]["content"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let text = rows
                .iter()
                .filter_map(|r| r[0]["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n");
            (text, Vec::new())
        }
        "interactive" => {
            let elements = value["elements"].as_array().cloned().unwrap_or_default();
            let text = elements[0]["content"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            let buttons = elements
                .iter()
                .flat_map(|e| e["actions"].as_array().cloned().unwrap_or_default())
                .map(|a| Button {
                    label: a["text"]["content"].as_str().unwrap_or_default().to_owned(),
                    data: a["value"]["prometheus"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                })
                .collect();
            (text, buttons)
        }
        _ => (String::new(), Vec::new()),
    }
}

fn fail(state: &mut State) -> Option<Reply> {
    Some(match state.fail_next.take()? {
        Failure::RateLimited(_) => Reply::json(
            429,
            &json!({"code": 99_991_400, "msg": "too many requests"}),
        ),
        Failure::ServerError => Reply::json(500, &json!({"code": 1, "msg": "internal error"})),
        Failure::Forbidden | Failure::Markup => Reply::json(
            400,
            &json!({"code": 230_002, "msg": "bot is not in the chat"}),
        ),
    })
}

fn ok(data: &Value) -> Reply {
    Reply::json(200, &json!({"code": 0, "msg": "success", "data": data}))
}

impl State {
    fn chat_of(&self, id: &str) -> String {
        self.origins
            .iter()
            .find(|(m, _)| m == id)
            .map(|(_, c)| c.clone())
            .or_else(|| {
                self.sent
                    .iter()
                    .find(|s| s.message_id == id)
                    .map(|s| s.chat_id.clone())
            })
            .unwrap_or_default()
    }

    fn create(&mut self, chat: &str, body: &Value) -> Reply {
        if let Some(reply) = fail(self) {
            return reply;
        }
        let kind = body["msg_type"].as_str().unwrap_or_default().to_owned();
        if kind == "post" && self.refuse_posts {
            return Reply::json(
                400,
                &json!({"code": 230_001, "msg": "invalid message content"}),
            );
        }
        let content = body["content"].as_str().unwrap_or_default();
        let id = format!("om_{}", self.sent.len() + self.files.len() + 1);
        let key: Value = serde_json::from_str(content).unwrap_or(Value::Null);
        let key = key["image_key"]
            .as_str()
            .or(key["file_key"].as_str())
            .unwrap_or_default();
        if let Some((_, name, bytes)) = self.uploads.iter().find(|(k, ..)| k == key).cloned() {
            let shown_as = match kind.as_str() {
                "image" => MediaKind::Image,
                "audio" => MediaKind::Voice,
                "media" => MediaKind::Video,
                _ => kind_of_name(&name),
            };
            self.files.push(MockFile {
                chat_id: chat.to_owned(),
                thread_id: None,
                name,
                kind: shown_as,
                bytes,
            });
        } else {
            let (text, buttons) = shown(&kind, content);
            self.sent.push(MockSent {
                chat_id: chat.to_owned(),
                message_id: id.clone(),
                text,
                buttons,
                thread_id: None,
            });
        }
        self.kinds.push((id.clone(), kind));
        ok(&json!({"message_id": id}))
    }

    fn upload(&mut self, request: &Request, prefix: &str) -> Reply {
        let (name, _, bytes) = catalog::multipart_file(&request.body).unwrap();
        let key = format!("{prefix}_{}", self.uploads.len() + 1);
        self.uploads.push((key.clone(), name, bytes));
        ok(&json!({"image_key": key, "file_key": key}))
    }

    fn edit(&mut self, id: &str, body: &Value) -> Reply {
        let kind = body["msg_type"].as_str().unwrap_or("interactive");
        let (text, _) = shown(kind, body["content"].as_str().unwrap_or_default());
        match self.sent.iter_mut().find(|s| s.message_id == id) {
            Some(sent) => {
                sent.text = text;
                ok(&json!({}))
            }
            None => Reply::json(400, &json!({"code": 230_011, "msg": "message not found"})),
        }
    }

    fn messages(&mut self, request: &Request, route: &str) -> Reply {
        let body = request.json();
        let rest = route.trim_start_matches("/open-apis/im/v1/messages");
        let parts: Vec<&str> = rest.trim_start_matches('/').split('/').collect();
        match (request.method.as_str(), parts.as_slice()) {
            ("POST", [""]) => self.create(body["receive_id"].as_str().unwrap_or_default(), &body),
            ("POST", [id, "reply"]) => {
                let chat = self.chat_of(id);
                let reply = self.create(&chat, &body);
                if reply.status == 200 {
                    let new =
                        serde_json::from_slice::<Value>(&reply.body).unwrap()["data"]["message_id"]
                            .as_str()
                            .unwrap()
                            .to_owned();
                    self.replies.push(((*id).to_owned(), new));
                }
                reply
            }
            ("PUT" | "PATCH", [id]) => self.edit(id, &body),
            ("DELETE", [id]) => {
                self.deleted.push((*id).to_owned());
                ok(&json!({}))
            }
            ("POST", [id, "reactions"]) => {
                let emoji = body["reaction_type"]["emoji_type"]
                    .as_str()
                    .unwrap_or_default();
                self.reactions.push(((*id).to_owned(), emoji.to_owned()));
                ok(&json!({"reaction_id": format!("r{}", self.reactions.len())}))
            }
            ("DELETE", [_, "reactions", _]) => ok(&json!({})),
            ("GET", [_, "resources", key]) => match self.resources.iter().find(|(k, _)| k == key) {
                Some((_, bytes)) => Reply {
                    status: 200,
                    headers: vec![("content-type".into(), "application/octet-stream".into())],
                    body: bytes.clone(),
                },
                None => Reply::json(404, &json!({"code": 234_003, "msg": "no such file"})),
            },
            _ => Reply::json(404, &json!({"code": 1, "msg": "no route"})),
        }
    }

    fn documents(&mut self, request: &Request) -> Reply {
        let route = request.route().to_owned();
        self.drive
            .push((request.method.clone(), request.path.clone(), request.json()));
        match (request.method.as_str(), route.as_str()) {
            ("GET", "/open-apis/docx/v1/documents/doc1/raw_content") => {
                ok(&json!({"content": "Quarterly plan: ship the adapter."}))
            }
            ("POST", "/open-apis/drive/v1/metas/batch_query") => {
                ok(&json!({"metas": [{"title": "Plan", "url": "https://x.feishu.cn/docx/doc1"}]}))
            }
            ("POST", "/open-apis/drive/v1/files/doc1/comments/batch_query") => ok(
                &json!({"items": [{"comment_id": "c1", "is_whole": self.whole, "quote": "ship the adapter"}]}),
            ),
            ("GET", "/open-apis/drive/v1/files/doc1/comments/c1/replies") => {
                ok(&json!({"items": self.thread, "has_more": false}))
            }
            ("GET", "/open-apis/drive/v1/files/doc1/comments") => ok(&json!({
                "items": [{"comment_id": "c1", "is_whole": true, "reply_list": {"replies": self.thread}}],
                "has_more": false,
            })),
            ("POST", "/open-apis/drive/v1/files/doc1/comments/c1/replies") => {
                ok(&json!({"reply_id": format!("r{}", self.drive.len())}))
            }
            ("POST", "/open-apis/drive/v1/files/doc1/new_comments") => {
                ok(&json!({"comment_id": format!("c{}", self.drive.len())}))
            }
            _ => Reply::json(404, &json!({"code": 1, "msg": "no route"})),
        }
    }
}

fn rest(port: u16, state: &Mutex<State>, request: &Request) -> Reply {
    let route = request.route().to_owned();
    let body = request.json();
    match (request.method.as_str(), route.as_str()) {
        ("POST", "/open-apis/auth/v3/tenant_access_token/internal") => {
            if body["app_id"] == APP_ID && body["app_secret"] == SECRET {
                Reply::json(
                    200,
                    &json!({"code": 0, "tenant_access_token": TOKEN, "expire": 7200}),
                )
            } else {
                Reply::json(400, &json!({"code": 10_014, "msg": "app secret invalid"}))
            }
        }
        ("POST", "/callback/ws/endpoint") if body["AppSecret"] == SECRET => Reply::json(
            200,
            &json!({"code": 0, "data": {"URL": format!("ws://127.0.0.1:{port}/ws?device_id=d1&service_id=7"),
                "ClientConfig": {"PingInterval": 120}}}),
        ),
        _ if request.header("authorization") != Some(&format!("Bearer {TOKEN}")) => {
            Reply::json(401, &json!({"code": 99_991_663, "msg": "invalid token"}))
        }
        ("GET", "/open-apis/bot/v3/info") => Reply::json(
            200,
            &json!({"code": 0, "bot": {"open_id": BOT, "app_name": "Prometheus"}}),
        ),
        ("POST", "/open-apis/im/v1/images") => state.lock().unwrap().upload(request, "img"),
        ("POST", "/open-apis/im/v1/files") => state.lock().unwrap().upload(request, "file"),
        ("GET", path) if path.starts_with("/open-apis/im/v1/chats/") => {
            let chat = path.rsplit('/').next().unwrap_or_default();
            let mode = if chat == DM { "p2p" } else { "group" };
            ok(&json!({"chat_mode": mode, "name": format!("chat {chat}")}))
        }
        (_, path) if path.starts_with("/open-apis/im/v1/messages") => {
            state.lock().unwrap().messages(request, path)
        }
        (_, path)
            if path.starts_with("/open-apis/drive/") || path.starts_with("/open-apis/docx/") =>
        {
            state.lock().unwrap().documents(request)
        }
        _ => Reply::json(404, &json!({"code": 1, "msg": "no route"})),
    }
}

impl Feishu {
    /// Starts the mock.
    pub async fn start() -> Arc<Self> {
        let state = Arc::new(Mutex::new(State::default()));
        let port = Arc::new(Mutex::new(0u16));
        let (on_open, on_frame, on_rest, rest_port) = (
            Arc::clone(&state),
            Arc::clone(&state),
            Arc::clone(&state),
            Arc::clone(&port),
        );
        let server = WsServer::start(
            Arc::new(move |socket: &Socket| {
                for frame in on_open.lock().unwrap().queued.drain(..) {
                    socket.send_binary(frame);
                }
                true
            }),
            Arc::new(move |_: &Socket, frame: &str| {
                use base64::Engine as _;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(frame)
                    .unwrap_or_default();
                let (method, headers) = frame_headers(&bytes);
                if method == 1
                    && let Some((_, id)) = headers.iter().find(|(k, _)| k == "message_id")
                {
                    on_frame.lock().unwrap().acks.push(id.clone());
                }
            }),
            Arc::new(move |request: &Request| rest(*rest_port.lock().unwrap(), &on_rest, request)),
        )
        .await;
        *port.lock().unwrap() = server.port;
        Arc::new(Self {
            server,
            state,
            next: AtomicU64::new(1),
        })
    }

    /// A fresh message id.
    pub fn id(&self) -> String {
        format!("om_in{}", self.next.fetch_add(1, Ordering::Relaxed))
    }

    /// Delivers `event` over the long connection (queued until a socket
    /// opens).
    pub fn deliver(&self, event: &Value) {
        let id = event["header"]["event_id"]
            .as_str()
            .unwrap_or("x")
            .to_owned();
        if let Some(message) = event["event"]["message"]["message_id"].as_str() {
            let chat = event["event"]["message"]["chat_id"]
                .as_str()
                .unwrap_or_default();
            self.state
                .lock()
                .unwrap()
                .origins
                .push((message.to_owned(), chat.to_owned()));
        }
        let frame = data_frame(&id, event.to_string().as_bytes());
        self.state.lock().unwrap().last = Some(frame.clone());
        if !self.server.broadcast_binary(&frame) {
            self.state.lock().unwrap().queued.push(frame);
        }
    }

    /// A text message from `sender` in `chat`; its id.
    pub fn say(&self, chat: &str, sender: &str, text: &str, mentions: &Value) -> String {
        let id = self.id();
        self.deliver(&message_event(
            &id,
            chat,
            sender,
            "text",
            &json!({"text": text}),
            mentions,
        ));
        id
    }

    /// A drive comment event: `from` commented, addressed to the bot.
    pub fn comment(&self, event_id: &str, from: &str) {
        self.deliver(&comment_event(event_id, from));
    }

    /// The plugin installed against this mock over `bridge`, with
    /// `extra` options.
    pub fn install(
        &self,
        bridge: Option<&Bridge>,
        extra: &Value,
    ) -> (
        prometheus::plugins_cmd::adapter_plugin::PluginKit,
        tempfile::TempDir,
    ) {
        let mut options = json!({"app_id": APP_ID, "api_base": self.server.base()});
        let mut hosts = vec![self.server.host()];
        if let Some(bridge) = bridge {
            options["sidecar"] = bridge.sidecar.clone();
            hosts.push(bridge.base.trim_start_matches("http://").to_owned());
        }
        for (k, v) in extra.as_object().into_iter().flatten() {
            options[k] = v.clone();
        }
        let (kit, root) = catalog::install("feishu", &options, &hosts);
        catalog::secret(&kit, "FEISHU_APP_SECRET", SECRET);
        (kit, root)
    }
}

/// A `drive.notice.comment_add_v1` event on comment `c1` of `doc1`.
pub fn comment_event(event_id: &str, from: &str) -> Value {
    envelope(
        event_id,
        "drive.notice.comment_add_v1",
        &json!({"comment_id": "c1", "reply_id": format!("r-{event_id}"), "is_mentioned": true,
            "notice_meta": {"file_token": "doc1", "file_type": "docx", "notice_type": "add_reply",
                "from_user_id": {"open_id": from}, "to_user_id": {"open_id": BOT}}}),
    )
}

impl MockPlatform for Feishu {
    fn dm(&self) -> (String, String) {
        (DM.to_owned(), USER.to_owned())
    }

    fn inject(&self, text: &str) -> String {
        self.say(DM, USER, text, &json!([]))
    }

    fn redeliver(&self) {
        let last = self.state.lock().unwrap().last.clone();
        if let Some(frame) = last
            && !self.server.broadcast_binary(&frame)
        {
            self.state.lock().unwrap().queued.push(frame);
        }
    }

    fn drop_connection(&self) {
        self.server.drop_all();
    }

    fn press(&self, message_id: &str, data: &str) {
        let chat = self.state.lock().unwrap().chat_of(message_id);
        let token = format!("c-{}", self.next.fetch_add(1, Ordering::Relaxed));
        self.deliver(&envelope(
            &format!("ev-{token}"),
            "card.action.trigger",
            &json!({"operator": {"open_id": USER}, "token": token,
                "action": {"tag": "button", "value": {"prometheus": data}},
                "context": {"open_message_id": message_id, "open_chat_id": chat}}),
        ));
    }

    fn fail_next(&self, failure: Failure) {
        self.state.lock().unwrap().fail_next = Some(failure);
    }

    fn reports_markup_errors(&self) -> bool {
        false
    }

    fn hints_retry_after(&self) -> bool {
        false
    }

    fn sent(&self) -> Vec<MockSent> {
        self.state.lock().unwrap().sent.clone()
    }

    fn typing(&self) -> usize {
        0
    }

    fn reactions(&self) -> Vec<(String, String)> {
        self.state.lock().unwrap().reactions.clone()
    }

    fn deleted(&self) -> Vec<String> {
        self.state.lock().unwrap().deleted.clone()
    }

    fn files(&self) -> Vec<MockFile> {
        self.state.lock().unwrap().files.clone()
    }

    fn inject_media(
        &self,
        kind: MediaKind,
        name: &str,
        bytes: &[u8],
        caption: &str,
    ) -> Option<String> {
        let key = format!("res_{}", self.next.fetch_add(1, Ordering::Relaxed));
        self.state
            .lock()
            .unwrap()
            .resources
            .push((key.clone(), bytes.to_vec()));
        let (message_type, content) = match kind {
            MediaKind::Image => ("image", json!({"image_key": key})),
            MediaKind::Voice => ("audio", json!({"file_key": key, "duration": 3000})),
            MediaKind::Video => ("media", json!({"file_key": key, "file_name": name})),
            _ => ("file", json!({"file_key": key, "file_name": name})),
        };
        let file_id = self.id();
        self.deliver(&message_event(
            &file_id,
            DM,
            USER,
            message_type,
            &content,
            &json!([]),
        ));
        Some(self.say(DM, USER, caption, &json!([])))
    }
}

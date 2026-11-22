//! The `qqbot` catalog adapter plugin (spec: platform-adapters, QQ Bot
//! adapter; Catalog adapter set) against a mock QQ Bot API: token, gateway
//! WebSocket (Hello, Identify, heartbeats, dispatches; held by the bridge
//! sidecar) and REST on one port: the conformance suite with message
//! keyboards, voice with QQ's own transcript, policies, numbered choices
//! without keyboards and chunked uploads.

#![cfg(feature = "gateway")]

mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use common::catalog::{self, Bridge, Reply, Request};
use common::catalog_ws::{Socket, WsServer};
use pmt_gateway::messaging::adapter::OutboundAttachment;
use pmt_gateway::messaging::conformance::{Failure, MockFile, MockPlatform, MockSent};
use pmt_gateway::messaging::{Button, MediaKind, Outgoing};
use serde_json::{Value, json};

const SECRET: &str = "qq-secret";
const USER: &str = "UOPENID1";

#[derive(Default)]
struct State {
    port: u16,
    queued: Vec<String>,
    last: Option<String>,
    seq: u64,
    identified: bool,
    sent: Vec<MockSent>,
    bodies: Vec<(String, Value)>,
    acks: Vec<String>,
    parts: usize,
    /// Files inbound messages point at: path, bytes.
    media: Vec<(String, Vec<u8>)>,
    /// Inline uploads: `file_info`, `file_type`, bytes.
    uploads: Vec<(String, u64, Vec<u8>)>,
    /// Files posted to chats.
    files: Vec<MockFile>,
    fail_next: Option<Failure>,
}

/// What a posted file of QQ `file_type` shows as.
fn shown(file_type: u64) -> MediaKind {
    match file_type {
        1 => MediaKind::Image,
        2 => MediaKind::Video,
        3 => MediaKind::Voice,
        _ => MediaKind::Document,
    }
}

struct QQ {
    server: WsServer,
    state: Arc<Mutex<State>>,
    next: AtomicU64,
}

fn rest(state: &Mutex<State>, request: &Request) -> Reply {
    let mut s = state.lock().unwrap();
    let route = request.route().to_owned();
    if route == "/app/getAppAccessToken" {
        return if request.json()["clientSecret"] == SECRET {
            Reply::json(200, &json!({"access_token": "AT", "expires_in": "7200"}))
        } else {
            Reply::json(401, &json!({"message": "invalid secret"}))
        };
    }
    if route.starts_with("/cos/") {
        s.parts += 1;
        return Reply::status(200);
    }
    if route.starts_with("/media/") {
        return match s.media.iter().find(|(path, _)| *path == route) {
            Some((_, bytes)) => Reply {
                status: 200,
                headers: Vec::new(),
                body: bytes.clone(),
            },
            None => Reply::status(404),
        };
    }
    if request.header("authorization") != Some("QQBot AT") {
        return Reply::json(401, &json!({"message": "unauthorized"}));
    }
    let body = request.json();
    match (request.method.as_str(), route.as_str()) {
        ("GET", "/gateway") => Reply::json(
            200,
            &json!({"url": format!("ws://127.0.0.1:{}/ws", s.port)}),
        ),
        ("PUT", path) if path.starts_with("/interactions/") => {
            s.acks
                .push(path.trim_start_matches("/interactions/").to_owned());
            Reply::json(200, &json!({}))
        }
        ("POST", path) if path.ends_with("/upload_prepare") => {
            let port = s.port;
            let parts: Vec<Value> = (1..=3)
                .map(|i| json!({"index": i, "presigned_url": format!("http://127.0.0.1:{port}/cos/{i}")}))
                .collect();
            Reply::json(
                200,
                &json!({"upload_id": "U1", "block_size": 5 * 1024 * 1024, "parts": parts}),
            )
        }
        ("POST", path) if path.ends_with("/upload_part_finish") => Reply::json(200, &json!({})),
        ("POST", path) if path.ends_with("/files") => upload_file(&mut s, path, &body),
        ("POST", path) if path.ends_with("/messages") => post_message(&mut s, path, &body),
        _ => Reply::status(404),
    }
}

/// An upload to a chat's `files`: inline bytes are kept for the message
/// that names their `file_info`.
fn upload_file(s: &mut State, path: &str, body: &Value) -> Reply {
    s.bodies.push((path.to_owned(), body.clone()));
    let Some(data) = body["file_data"].as_str() else {
        return Reply::json(
            200,
            &json!({"file_info": "FINFO", "file_uuid": "f1", "ttl": 0}),
        );
    };
    let info = format!("FINFO-{}", s.uploads.len() + 1);
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .unwrap();
    s.uploads
        .push((info.clone(), body["file_type"].as_u64().unwrap_or(4), bytes));
    Reply::json(
        200,
        &json!({"file_info": info, "file_uuid": "f1", "ttl": 0}),
    )
}

/// A message to a chat: recorded, with the file it names.
fn post_message(s: &mut State, path: &str, body: &Value) -> Reply {
    match s.fail_next.take() {
        Some(Failure::RateLimited(_)) => return Reply::json(429, &json!({"code": 22009})),
        Some(Failure::ServerError) => return Reply::status(500),
        Some(Failure::Forbidden | Failure::Markup) => {
            return Reply::json(403, &json!({"code": 11298}));
        }
        None => {}
    }
    let chat = if let Some(id) = path.strip_prefix("/v2/users/") {
        format!("c2c:{}", id.trim_end_matches("/messages"))
    } else {
        format!(
            "group:{}",
            path.trim_start_matches("/v2/groups/")
                .trim_end_matches("/messages")
        )
    };
    let id = format!("out{}", s.sent.len() + 1);
    let info = body["media"]["file_info"].as_str().unwrap_or_default();
    if let Some((_, file_type, bytes)) = s.uploads.iter().find(|u| u.0 == info).cloned() {
        s.files.push(MockFile {
            chat_id: chat.clone(),
            thread_id: None,
            name: info.to_owned(),
            kind: shown(file_type),
            bytes,
        });
    }
    let buttons = body["keyboard"]["content"]["rows"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|r| r["buttons"].as_array().cloned().unwrap_or_default())
        .map(|b| pmt_gateway::messaging::Button {
            label: b["render_data"]["label"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            data: b["action"]["data"].as_str().unwrap_or_default().to_owned(),
        })
        .collect();
    let text = body["content"]
        .as_str()
        .or(body["markdown"]["content"].as_str())
        .unwrap_or_default()
        .to_owned();
    s.bodies.push((path.to_owned(), body.clone()));
    s.sent.push(MockSent {
        chat_id: chat,
        message_id: id.clone(),
        text,
        buttons,
        thread_id: None,
    });
    Reply::json(200, &json!({"id": id, "timestamp": "0"}))
}

fn on_frame(state: &Mutex<State>, socket: &Socket, frame: &str) {
    let frame: Value = serde_json::from_str(frame).unwrap();
    let mut s = state.lock().unwrap();
    match frame["op"].as_u64() {
        Some(2 | 6) if frame["d"]["token"] == "QQBot AT" => {
            s.identified = true;
            s.seq += 1;
            let kind = if frame["op"] == 2 { "READY" } else { "RESUMED" };
            socket.send(
                json!({"op": 0, "s": s.seq, "t": kind, "d": {"session_id": "S1"}}).to_string(),
            );
            for queued in s.queued.drain(..) {
                socket.send(queued);
            }
        }
        Some(1) => socket.send(json!({"op": 11}).to_string()),
        _ => {}
    }
}

impl QQ {
    async fn start() -> Arc<Self> {
        let state = Arc::new(Mutex::new(State::default()));
        let (on_frames, for_rest) = (Arc::clone(&state), Arc::clone(&state));
        let server = WsServer::start(
            Arc::new(|socket: &Socket| {
                socket.send(json!({"op": 10, "d": {"heartbeat_interval": 41250}}).to_string());
                true
            }),
            Arc::new(move |socket: &Socket, frame: &str| on_frame(&on_frames, socket, frame)),
            Arc::new(move |request: &Request| rest(&for_rest, request)),
        )
        .await;
        state.lock().unwrap().port = server.port;
        Arc::new(Self {
            server,
            state,
            next: AtomicU64::new(1),
        })
    }

    fn dispatch(&self, kind: &str, data: &Value) {
        let frame = {
            let mut s = self.state.lock().unwrap();
            s.seq += 1;
            json!({"op": 0, "s": s.seq, "t": kind, "d": data}).to_string()
        };
        self.state.lock().unwrap().last = Some(frame.clone());
        let ready = self.state.lock().unwrap().identified;
        if !(ready && self.server.broadcast(&frame)) {
            self.state.lock().unwrap().queued.push(frame);
        }
    }

    fn c2c(&self, user: &str, content: &str, attachments: &Value) -> String {
        let id = format!("m{}", self.next.fetch_add(1, Ordering::Relaxed));
        self.dispatch(
            "C2C_MESSAGE_CREATE",
            &json!({"id": id, "content": content, "author": {"user_openid": user}, "attachments": attachments}),
        );
        id
    }

    fn install(
        &self,
        bridge: &Bridge,
        extra: &Value,
    ) -> (
        prometheus::plugins_cmd::adapter_plugin::PluginKit,
        tempfile::TempDir,
    ) {
        let mut options = json!({
            "app_id": "1020",
            "api_base": self.server.base(),
            "token_url": format!("{}/app/getAppAccessToken", self.server.base()),
            "sidecar": bridge.sidecar,
        });
        for (k, v) in extra.as_object().into_iter().flatten() {
            options[k] = v.clone();
        }
        let hosts = [
            self.server.host(),
            bridge.base.trim_start_matches("http://").to_owned(),
        ];
        let (kit, root) = catalog::install("qqbot", &options, &hosts);
        catalog::secret(&kit, "QQBOT_CLIENT_SECRET", SECRET);
        (kit, root)
    }
}

impl MockPlatform for QQ {
    fn dm(&self) -> (String, String) {
        (format!("c2c:{USER}"), USER.to_owned())
    }

    fn inject(&self, text: &str) -> String {
        self.c2c(USER, text, &json!([]))
    }

    fn redeliver(&self) {
        let last = self.state.lock().unwrap().last.clone();
        if let Some(frame) = last {
            self.server.broadcast(&frame);
        }
    }

    fn drop_connection(&self) {
        self.state.lock().unwrap().identified = false;
        self.server.drop_all();
    }

    fn press(&self, _message_id: &str, data: &str) {
        let id = format!("i{}", self.next.fetch_add(1, Ordering::Relaxed));
        self.dispatch(
            "INTERACTION_CREATE",
            &json!({"id": id, "user_openid": USER, "data": {"resolved": {"button_data": data}}}),
        );
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
        Vec::new()
    }

    /// A C2C message with the caption and one attachment QQ hosts (a
    /// voice clip without QQ's own transcript, so the clip itself comes).
    fn inject_media(
        &self,
        kind: MediaKind,
        name: &str,
        bytes: &[u8],
        caption: &str,
    ) -> Option<String> {
        let content_type = match kind {
            MediaKind::Image => "image/png",
            MediaKind::Video => "video/mp4",
            MediaKind::Voice => "voice",
            _ => "file",
        };
        let path = {
            let mut s = self.state.lock().unwrap();
            let path = format!("/media/{}", s.media.len() + 1);
            s.media.push((path.clone(), bytes.to_vec()));
            path
        };
        let url = format!("{}{path}", self.server.base());
        Some(self.c2c(
            USER,
            caption,
            &json!([{"content_type": content_type, "url": url, "filename": name}]),
        ))
    }

    fn files(&self) -> Vec<MockFile> {
        self.state.lock().unwrap().files.clone()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_qqbot_adapter_passes_the_conformance_suite() {
    let qq = QQ::start().await;
    let bridge = Bridge::start(&[qq.server.host()]);
    let (kit, _root) = qq.install(&bridge, &json!({"keyboards": true}));
    let report = kit
        .run(
            kit.adapter().unwrap(),
            Arc::clone(&qq) as Arc<dyn MockPlatform>,
        )
        .await;
    catalog::assert_passed(&report);
}

/// Scenario: Voice with platform transcript.
#[tokio::test(flavor = "multi_thread")]
async fn a_voice_message_with_a_transcript_needs_no_speech_to_text() {
    let qq = QQ::start().await;
    let bridge = Bridge::start(&[qq.server.host()]);
    let (kit, _root) = qq.install(&bridge, &json!({}));
    let adapter = kit.adapter().unwrap();
    let link = catalog::Link::new();
    adapter.connect(link.dyn_link()).await.unwrap();
    qq.c2c(
        USER,
        "",
        &json!([{"content_type": "voice", "url": format!("{}/voice.silk", qq.server.base()), "filename": "v.silk", "asr_refer_text": "what's the weather"}]),
    );
    let admitted = link.wait_for(1).await;
    assert_eq!(admitted.len(), 1);
    assert_eq!(admitted[0].text, "what's the weather");
    // No clip reaches the gateway, so its speech-to-text is never called.
    assert!(admitted[0].attachments.is_empty());
    adapter.disconnect().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn policies_numbered_choices_and_chunked_uploads() {
    let qq = QQ::start().await;
    let bridge = Bridge::start(&[qq.server.host()]);
    let (kit, _root) = qq.install(
        &bridge,
        &json!({"dm_policy": "allowlist", "allow_from": [USER]}),
    );
    let adapter = kit.adapter().unwrap();
    let link = catalog::Link::new();
    adapter.connect(link.dyn_link()).await.unwrap();
    qq.c2c("STRANGER", "hi", &json!([]));
    qq.c2c(USER, "approve?", &json!([]));
    let admitted = link.wait_for(1).await;
    assert_eq!(admitted.len(), 1, "the stranger is dropped");
    let chat = format!("c2c:{USER}");
    // Without keyboards the choices are numbered, and "2" picks the second.
    let mut ask = Outgoing::text(&chat, None, "Run it?");
    ask.buttons = vec![
        Button {
            label: "Yes".into(),
            data: "yes".into(),
        },
        Button {
            label: "No".into(),
            data: "no".into(),
        },
    ];
    adapter.send(&ask).await.unwrap();
    assert!(qq.sent()[0].text.contains("2. No"), "{}", qq.sent()[0].text);
    assert_eq!(
        qq.state.lock().unwrap().bodies[0].1["msg_id"],
        admitted[0].message_id
    );
    qq.c2c(USER, "2", &json!([]));
    let picked = link.wait_for(2).await;
    assert_eq!(picked[1].callback.as_deref(), Some("no"));
    // A 12 MB file goes up in parts.
    let dir = catalog::scratch();
    let path = dir.path().join("big.bin");
    std::fs::write(&path, vec![7u8; 12 * 1024 * 1024]).unwrap();
    let mut out = Outgoing::text(&chat, None, "");
    out.attachments
        .push(OutboundAttachment::new(&path.display().to_string()));
    adapter.send(&out).await.unwrap();
    {
        let s = qq.state.lock().unwrap();
        assert_eq!(s.parts, 3);
        assert_eq!(s.bodies.last().unwrap().1["media"]["file_info"], "FINFO");
    }
    adapter.disconnect().await;
}

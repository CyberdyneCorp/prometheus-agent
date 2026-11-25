//! The `wecom` catalog adapter plugin (spec: platform-adapters, WeCom
//! smart robot adapter; Catalog adapter set) against a mock AI Bot
//! WebSocket (held by the bridge sidecar): the conformance suite (replies
//! as native streams that edits update), per-group sender allowlists,
//! encrypted inbound media and chunked uploads within the limits.

#![cfg(feature = "gateway")]

mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use common::catalog::{self, Bridge, MockHttp, Reply, Request};
use common::catalog_crypt::cbc_encrypt;
use common::catalog_ws::{Socket, WsServer};
use pmt_gateway::messaging::adapter::OutboundAttachment;
use pmt_gateway::messaging::conformance::{
    Failure, MockFile, MockPlatform, MockSent, kind_of_name,
};
use pmt_gateway::messaging::{MediaKind, Outgoing};
use serde_json::{Value, json};

const SECRET: &str = "wecom-bot-secret";
const USER: &str = "alice";

#[derive(Default)]
struct State {
    queued: Vec<String>,
    last: Option<String>,
    /// Streams and proactive messages: id, chat, text.
    sent: Vec<MockSent>,
    /// Stream frames: (stream id, finish).
    frames: Vec<(String, bool)>,
    /// Callback `req_id` -> chat.
    callbacks: Vec<(String, String)>,
    uploads: Vec<(String, usize)>,
    chunks: usize,
    /// The upload in progress: its file name and the chunks so far.
    uploading: (String, Vec<u8>),
    /// Finished uploads: media id, name, bytes.
    media: Vec<(String, String, Vec<u8>)>,
    /// Files posted to chats.
    files: Vec<MockFile>,
    /// Encrypted files the CDN serves: path, sealed bytes.
    cdn: Vec<(String, Vec<u8>)>,
    fail_next: Option<Failure>,
}

/// The AES key of every file the mock CDN serves.
const CDN_KEY: [u8; 32] = [7; 32];

/// What a posted media message shows as.
fn shown(msgtype: &str, name: &str) -> MediaKind {
    match msgtype {
        "image" => MediaKind::Image,
        "video" => MediaKind::Video,
        "voice" => MediaKind::Voice,
        _ => kind_of_name(name),
    }
}

struct WeCom {
    server: WsServer,
    cdn: MockHttp,
    state: Arc<Mutex<State>>,
    next: AtomicU64,
}

fn ack(req_id: &Value, errcode: i64, body: &Value) -> String {
    json!({"headers": {"req_id": req_id}, "errcode": errcode, "errmsg": "ok", "body": body})
        .to_string()
}

fn failure(state: &mut State) -> Option<i64> {
    Some(match state.fail_next.take()? {
        Failure::RateLimited(_) => 45009,
        Failure::ServerError => -1,
        Failure::Forbidden | Failure::Markup => 60020,
    })
}

fn on_frame(state: &Mutex<State>, socket: &Socket, frame: &str) {
    let frame: Value = serde_json::from_str(frame).unwrap();
    let req_id = &frame["headers"]["req_id"];
    let body = &frame["body"];
    let mut state = state.lock().unwrap();
    let answer = match frame["cmd"].as_str().unwrap_or_default() {
        "aibot_subscribe" => {
            let ok = body["secret"] == SECRET;
            ack(req_id, if ok { 0 } else { 40001 }, &json!({}))
        }
        "ping" => ack(req_id, 0, &json!({})),
        "aibot_respond_msg" => {
            // A refusal hits the frame carrying the reply, not the one
            // closing the chat's previous stream.
            let closing = body["stream"]["finish"] == true;
            if let Some(code) = (!closing).then(|| failure(&mut state)).flatten() {
                return socket.send(ack(req_id, code, &json!({})));
            }
            let stream = &body["stream"];
            let id = stream["id"].as_str().unwrap_or_default().to_owned();
            let text = stream["content"].as_str().unwrap_or_default().to_owned();
            let chat = state
                .callbacks
                .iter()
                .find(|(r, _)| r == req_id)
                .map(|(_, c)| c.clone())
                .unwrap_or_default();
            state.frames.push((id.clone(), stream["finish"] == true));
            match state.sent.iter_mut().find(|s| s.message_id == id) {
                Some(sent) => sent.text = text,
                None => state.sent.push(MockSent {
                    chat_id: chat,
                    message_id: id,
                    text,
                    buttons: Vec::new(),
                    thread_id: None,
                }),
            }
            ack(req_id, 0, &json!({}))
        }
        "aibot_send_msg" => {
            if let Some(code) = failure(&mut state) {
                return socket.send(ack(req_id, code, &json!({})));
            }
            let kind = body["msgtype"].as_str().unwrap_or_default();
            let chat = body["chatid"].as_str().unwrap_or_default().to_owned();
            let media_id = body[kind]["media_id"].as_str().unwrap_or_default();
            if let Some((_, name, bytes)) = state.media.iter().find(|m| m.0 == media_id).cloned() {
                state.files.push(MockFile {
                    chat_id: chat.clone(),
                    thread_id: None,
                    kind: shown(kind, &name),
                    name,
                    bytes,
                });
            }
            let text = match kind {
                "markdown" => body["markdown"]["content"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                other => format!(
                    "[{other}:{}]",
                    body[other]["media_id"].as_str().unwrap_or_default()
                ),
            };
            let id = format!("p{}", state.sent.len() + 1);
            state.sent.push(MockSent {
                chat_id: chat,
                message_id: id,
                text,
                buttons: Vec::new(),
                thread_id: None,
            });
            ack(req_id, 0, &json!({}))
        }
        cmd if cmd.starts_with("aibot_upload_media_") => upload(&mut state, cmd, req_id, body),
        _ => return,
    };
    socket.send(answer);
}

/// The chunked upload: init names the file, chunks carry base64, finish
/// answers its media id.
fn upload(state: &mut State, cmd: &str, req_id: &Value, body: &Value) -> String {
    match cmd {
        "aibot_upload_media_init" => {
            let kind = body["type"].as_str().unwrap_or_default().to_owned();
            state.uploads.push((
                kind,
                usize::try_from(body["total_chunks"].as_u64().unwrap_or(0)).unwrap(),
            ));
            state.uploading = (
                body["filename"].as_str().unwrap_or_default().to_owned(),
                Vec::new(),
            );
            ack(req_id, 0, &json!({"upload_id": "up1"}))
        }
        "aibot_upload_media_chunk" => {
            state.chunks += 1;
            let chunk = base64::engine::general_purpose::STANDARD
                .decode(body["base64_data"].as_str().unwrap_or_default())
                .unwrap();
            state.uploading.1.extend(chunk);
            ack(req_id, 0, &json!({}))
        }
        "aibot_upload_media_finish" => {
            let media_id = format!("media-{}", state.media.len() + 1);
            let (name, bytes) = std::mem::take(&mut state.uploading);
            state.media.push((media_id.clone(), name, bytes));
            ack(req_id, 0, &json!({"media_id": media_id, "type": "file"}))
        }
        _ => ack(req_id, 40000, &json!({})),
    }
}

impl WeCom {
    async fn start(rest: catalog::Handler) -> Arc<Self> {
        let state = Arc::new(Mutex::new(State::default()));
        let (on_open, frames, for_cdn) =
            (Arc::clone(&state), Arc::clone(&state), Arc::clone(&state));
        let cdn = MockHttp::start(Arc::new(move |request: &Request| {
            let state = for_cdn.lock().unwrap();
            match state.cdn.iter().find(|(path, _)| *path == request.path) {
                Some((_, sealed)) => Reply {
                    status: 200,
                    headers: Vec::new(),
                    body: sealed.clone(),
                },
                None => Reply::status(404),
            }
        }));
        let server = WsServer::start(
            Arc::new(move |socket: &Socket| {
                for frame in on_open.lock().unwrap().queued.drain(..) {
                    socket.send(frame);
                }
                true
            }),
            Arc::new(move |socket: &Socket, frame: &str| on_frame(&frames, socket, frame)),
            rest,
        )
        .await;
        Arc::new(Self {
            server,
            cdn,
            state,
            next: AtomicU64::new(1),
        })
    }

    fn deliver(&self, frame: String) {
        self.state.lock().unwrap().last = Some(frame.clone());
        if !self.server.broadcast(&frame) {
            self.state.lock().unwrap().queued.push(frame);
        }
    }

    /// A callback in `chat` (a group when `group`) from `sender` with
    /// `body` fields: its message id.
    fn callback(&self, chat: &str, group: bool, sender: &str, fields: &Value) -> String {
        let n = self.next.fetch_add(1, Ordering::Relaxed);
        let (id, req_id) = (format!("msg{n}"), format!("cb{n}"));
        let mut body = json!({
            "msgid": id, "chattype": if group { "group" } else { "single" },
            "from": {"userid": sender},
        });
        if group {
            body["chatid"] = json!(chat);
        }
        for (k, v) in fields.as_object().into_iter().flatten() {
            body[k] = v.clone();
        }
        self.state
            .lock()
            .unwrap()
            .callbacks
            .push((req_id.clone(), chat.to_owned()));
        self.deliver(
            json!({"cmd": "aibot_msg_callback", "headers": {"req_id": req_id}, "body": body})
                .to_string(),
        );
        id
    }

    fn text(&self, chat: &str, group: bool, sender: &str, text: &str) -> String {
        self.callback(
            chat,
            group,
            sender,
            &json!({"msgtype": "text", "text": {"content": text}}),
        )
    }

    fn install(
        &self,
        bridge: &Bridge,
        extra: &Value,
        hosts: &[String],
    ) -> (
        prometheus::plugins_cmd::adapter_plugin::PluginKit,
        tempfile::TempDir,
    ) {
        let mut options =
            json!({"bot_id": "bot1", "websocket_url": self.server.ws(), "sidecar": bridge.sidecar});
        for (k, v) in extra.as_object().into_iter().flatten() {
            options[k] = v.clone();
        }
        let mut granted = vec![
            self.server.host(),
            self.cdn.host(),
            bridge.base.trim_start_matches("http://").to_owned(),
        ];
        granted.extend_from_slice(hosts);
        let (kit, root) = catalog::install("wecom", &options, &granted);
        catalog::secret(&kit, "WECOM_BOT_SECRET", SECRET);
        (kit, root)
    }
}

impl MockPlatform for WeCom {
    fn dm(&self) -> (String, String) {
        (USER.to_owned(), USER.to_owned())
    }

    fn inject(&self, text: &str) -> String {
        self.text(USER, false, USER, text)
    }

    fn redeliver(&self) {
        let last = self.state.lock().unwrap().last.clone();
        if let Some(frame) = last {
            self.deliver(frame);
        }
    }

    fn drop_connection(&self) {
        self.server.drop_all();
    }

    fn press(&self, _message_id: &str, _data: &str) {}

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

    /// A `mixed` callback: the caption and the file, sealed on the CDN
    /// with its own key as WeCom does.
    fn inject_media(
        &self,
        kind: MediaKind,
        name: &str,
        bytes: &[u8],
        caption: &str,
    ) -> Option<String> {
        let (msgtype, field) = match kind {
            MediaKind::Image => ("image", "image"),
            MediaKind::Video => ("video", "video"),
            _ => ("file", "file"),
        };
        let path = {
            let mut state = self.state.lock().unwrap();
            let path = format!("/cdn/{}", state.cdn.len() + 1);
            state.cdn.push((path.clone(), cbc_encrypt(&CDN_KEY, bytes)));
            path
        };
        let aeskey = base64::engine::general_purpose::STANDARD.encode(CDN_KEY);
        let item = json!({"msgtype": msgtype, field: {
            "url": format!("{}{path}", self.cdn.base()),
            "aeskey": aeskey.trim_end_matches('='),
            "filename": name,
        }});
        let text = json!({"msgtype": "text", "text": {"content": caption}});
        Some(self.callback(
            USER,
            false,
            USER,
            &json!({"msgtype": "mixed", "mixed": {"msg_item": [text, item]}}),
        ))
    }

    fn files(&self) -> Vec<MockFile> {
        self.state.lock().unwrap().files.clone()
    }

    /// Voice goes out only as AMR; anything else arrives as a file.
    fn voice_notes_out(&self) -> bool {
        false
    }
}

fn no_rest() -> catalog::Handler {
    Arc::new(|_: &Request| Reply::status(404))
}

#[tokio::test(flavor = "multi_thread")]
async fn the_wecom_adapter_passes_the_conformance_suite() {
    let wecom = WeCom::start(no_rest()).await;
    let bridge = Bridge::start(&[wecom.server.host()]);
    let (kit, _root) = wecom.install(&bridge, &json!({}), &[]);
    let report = kit
        .run(
            kit.adapter().unwrap(),
            Arc::clone(&wecom) as Arc<dyn MockPlatform>,
        )
        .await;
    catalog::assert_passed(&report);
}

/// Scenario: Per-group sender allowlist.
#[tokio::test(flavor = "multi_thread")]
async fn a_sender_outside_the_group_allowlist_is_not_admitted() {
    let wecom = WeCom::start(no_rest()).await;
    let bridge = Bridge::start(&[wecom.server.host()]);
    let (kit, _root) = wecom.install(
        &bridge,
        &json!({"groups": {"G1": {"allow_from": ["alice"]}}}),
        &[],
    );
    let adapter = kit.adapter().unwrap();
    let link = catalog::Link::new();
    adapter.connect(link.dyn_link()).await.unwrap();
    wecom.text("G1", true, "bob", "@Bot let me in");
    wecom.text("G1", true, "alice", "@Bot hello");
    let admitted = link.wait_for(1).await;
    let seen: Vec<(&str, &str)> = admitted
        .iter()
        .map(|i| (i.sender_id.as_str(), i.text.as_str()))
        .collect();
    assert_eq!(seen, [("alice", "hello")]);
    adapter.disconnect().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn encrypted_media_is_decrypted_and_uploads_respect_the_limits() {
    let key = [4u8; 32];
    let sealed = cbc_encrypt(&key, b"PNGDATA");
    let cdn = MockHttp::start(Arc::new(move |_: &Request| Reply {
        status: 200,
        headers: Vec::new(),
        body: sealed.clone(),
    }));
    let wecom = WeCom::start(no_rest()).await;
    let bridge = Bridge::start(&[wecom.server.host()]);
    let (kit, _root) = wecom.install(&bridge, &json!({}), &[cdn.host()]);
    let adapter = kit.adapter().unwrap();
    let link = catalog::Link::new();
    adapter.connect(link.dyn_link()).await.unwrap();
    let aeskey = base64::engine::general_purpose::STANDARD.encode(key);
    wecom.callback(
        USER,
        false,
        USER,
        &json!({"msgtype": "image", "image": {"url": format!("{}/img", cdn.base()), "aeskey": aeskey.trim_end_matches('=')}}),
    );
    let admitted = link.wait_for(1).await;
    assert_eq!(admitted.len(), 1);
    let mut stream = adapter
        .open_media(&admitted[0].attachments[0])
        .await
        .unwrap();
    let mut bytes = Vec::new();
    while let Some(chunk) = futures_util::StreamExt::next(&mut stream.chunks).await {
        bytes.extend(chunk.unwrap());
    }
    assert_eq!(bytes, b"PNGDATA");
    // An 11 MB image goes as a file, in 512 KB chunks, after a notice.
    let dir = catalog::scratch();
    let path = dir.path().join("big.png");
    std::fs::write(&path, vec![1u8; 11 * 1024 * 1024]).unwrap();
    let mut out = Outgoing::text(USER, None, "");
    out.attachments
        .push(OutboundAttachment::new(&path.display().to_string()));
    adapter.send(&out).await.unwrap();
    {
        let state = wecom.state.lock().unwrap();
        assert_eq!(state.uploads, [("file".to_owned(), 22)]);
        assert_eq!(state.chunks, 22);
        let texts: Vec<&str> = state.sent.iter().map(|s| s.text.as_str()).collect();
        assert!(texts[0].contains("10 MB image limit"), "{texts:?}");
        assert_eq!(texts[1], "[file:media-1]");
    }
    adapter.disconnect().await;
}

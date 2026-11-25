//! The `weixin` catalog adapter plugin (spec: platform-adapters, Weixin
//! personal WeChat adapter; Catalog adapter set) against a mock iLink bot
//! API and CDN: the conformance suite (the long poll held by the bridge),
//! the context token kept across a restart, the stale-token retry,
//! session-expiry pause and AES-128-ECB media both ways.

#![cfg(feature = "gateway")]

mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use common::catalog::{self, Bridge, MockHttp, Reply, Request};
use common::catalog_crypt::{ecb_decrypt, ecb_encrypt};
use pmt_gateway::messaging::adapter::OutboundAttachment;
use pmt_gateway::messaging::conformance::{
    Failure, MockFile, MockPlatform, MockSent, kind_of_name,
};
use pmt_gateway::messaging::{MediaKind, Outgoing};
use serde_json::{Value, json};

const TOKEN: &str = "ilink-token";
const ACCOUNT: &str = "bot@im.bot";
const PEER: &str = "peer@im.wechat";

#[derive(Default)]
struct State {
    queued: Vec<Value>,
    last: Option<Value>,
    cursor: u64,
    /// Sends: (to, context token, items).
    sends: Vec<(String, Option<String>, Vec<Value>)>,
    sent: Vec<MockSent>,
    /// Peers whose context token the mock considers stale.
    stale: Vec<String>,
    expire_next: bool,
    uploads: Vec<Vec<u8>>,
    files: Vec<(String, Vec<u8>)>,
    /// Files sent to peers, decrypted with the key their item names.
    delivered: Vec<MockFile>,
    fail_next: Option<Failure>,
}

/// The key of every file the mock delivers.
const CDN_KEY: [u8; 16] = [8; 16];

/// A CDN key as items carry it: base64 of 32 hex characters.
fn item_key(text: &str) -> [u8; 16] {
    let hex = base64::engine::general_purpose::STANDARD
        .decode(text)
        .unwrap();
    let key: Vec<u8> = (0..16)
        .map(|i| {
            u8::from_str_radix(std::str::from_utf8(&hex[i * 2..i * 2 + 2]).unwrap(), 16).unwrap()
        })
        .collect();
    key.try_into().unwrap()
}

/// The file a sent media item names, decrypted.
fn delivered(s: &State, to: &str, item: &Value) -> Option<MockFile> {
    let (kind, entry) = match item["type"].as_i64()? {
        2 => (MediaKind::Image, &item["image_item"]),
        5 => (MediaKind::Video, &item["video_item"]),
        4 => (MediaKind::Document, &item["file_item"]),
        _ => return None,
    };
    let media = &entry["media"];
    let index: usize = media["encrypt_query_param"]
        .as_str()?
        .strip_prefix("stored-")?
        .parse()
        .ok()?;
    let sealed = s.uploads.get(index - 1)?;
    let name = entry["file_name"].as_str().unwrap_or("media").to_owned();
    let kind = if kind == MediaKind::Document {
        kind_of_name(&name)
    } else {
        kind
    };
    Some(MockFile {
        chat_id: to.to_owned(),
        thread_id: None,
        name,
        kind,
        bytes: ecb_decrypt(&item_key(media["aes_key"].as_str()?), sealed),
    })
}

struct ILinkMock {
    api: MockHttp,
    state: Arc<Mutex<State>>,
    next: AtomicU64,
}

fn updates(state: &Mutex<State>) -> Reply {
    for _ in 0..20 {
        let mut s = state.lock().unwrap();
        if s.expire_next {
            s.expire_next = false;
            return Reply::json(200, &json!({"ret": -14, "errmsg": "session timeout"}));
        }
        if !s.queued.is_empty() {
            s.cursor += 1;
            let msgs: Vec<Value> = s.queued.drain(..).collect();
            return Reply::json(
                200,
                &json!({"ret": 0, "msgs": msgs, "get_updates_buf": format!("buf{}", s.cursor)}),
            );
        }
        drop(s);
        std::thread::sleep(Duration::from_millis(50));
    }
    let cursor = state.lock().unwrap().cursor;
    Reply::json(
        200,
        &json!({"ret": 0, "msgs": [], "get_updates_buf": format!("buf{cursor}")}),
    )
}

fn send(state: &Mutex<State>, request: &Request) -> Reply {
    let mut s = state.lock().unwrap();
    match s.fail_next.take() {
        Some(Failure::RateLimited(_)) => return Reply::json(429, &json!({"errmsg": "slow down"})),
        Some(Failure::ServerError) => return Reply::status(502),
        Some(Failure::Forbidden | Failure::Markup) => {
            return Reply::json(200, &json!({"ret": 1, "errmsg": "blocked"}));
        }
        None => {}
    }
    let message = &request.json()["msg"];
    let to = message["to_user_id"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let token = message["context_token"].as_str().map(str::to_owned);
    if token.is_some() && s.stale.contains(&to) {
        return Reply::json(200, &json!({"ret": -2, "errmsg": "prepare failed"}));
    }
    let items = message["item_list"].as_array().cloned().unwrap_or_default();
    let files: Vec<MockFile> = items.iter().filter_map(|i| delivered(&s, &to, i)).collect();
    s.delivered.extend(files);
    s.sends.push((to.clone(), token, items.clone()));
    let text = items
        .iter()
        .filter_map(|i| i["text_item"]["text"].as_str())
        .collect::<Vec<_>>()
        .join("");
    s.sent.push(MockSent {
        chat_id: to,
        message_id: message["client_id"].as_str().unwrap_or_default().to_owned(),
        text,
        buttons: Vec::new(),
        thread_id: None,
    });
    Reply::json(200, &json!({"ret": 0}))
}

fn answer(state: &Mutex<State>, request: &Request) -> Reply {
    let route = request.route().to_owned();
    if route.starts_with("/ilink/")
        && request.header("authorization") != Some(&format!("Bearer {TOKEN}"))
    {
        return Reply::json(401, &json!({"errmsg": "unauthorized"}));
    }
    match route.as_str() {
        "/ilink/bot/getupdates" => updates(state),
        "/ilink/bot/sendmessage" => send(state, request),
        "/ilink/bot/getuploadurl" => {
            Reply::json(200, &json!({"ret": 0, "upload_param": "up-param"}))
        }
        "/c2c/upload" => {
            let mut s = state.lock().unwrap();
            s.uploads.push(request.body.clone());
            Reply {
                status: 200,
                headers: vec![(
                    "x-encrypted-param".into(),
                    format!("stored-{}", s.uploads.len()),
                )],
                body: Vec::new(),
            }
        }
        "/c2c/download" => {
            let s = state.lock().unwrap();
            let param = request
                .path
                .split("encrypted_query_param=")
                .nth(1)
                .unwrap_or_default();
            match s.files.iter().find(|(p, _)| p == param) {
                Some((_, bytes)) => Reply {
                    status: 200,
                    headers: Vec::new(),
                    body: bytes.clone(),
                },
                None => Reply::status(404),
            }
        }
        _ => Reply::status(404),
    }
}

impl ILinkMock {
    fn start() -> Arc<Self> {
        let state = Arc::new(Mutex::new(State::default()));
        let served = Arc::clone(&state);
        let api = MockHttp::start(Arc::new(move |request: &Request| answer(&served, request)));
        Arc::new(Self {
            api,
            state,
            next: AtomicU64::new(1),
        })
    }

    fn options(&self, bridge: &Bridge) -> Value {
        json!({
            "account_id": ACCOUNT,
            "base_url": self.api.base(),
            "cdn_base_url": format!("{}/c2c", self.api.base()),
            "sidecar": bridge.sidecar,
        })
    }

    fn install(
        &self,
        bridge: &Bridge,
    ) -> (
        prometheus::plugins_cmd::adapter_plugin::PluginKit,
        tempfile::TempDir,
    ) {
        let hosts = [
            self.api.host(),
            bridge.base.trim_start_matches("http://").to_owned(),
        ];
        let (kit, root) = catalog::install("weixin", &self.options(bridge), &hosts);
        catalog::secret(&kit, "WEIXIN_TOKEN", TOKEN);
        (kit, root)
    }

    fn deliver(&self, message: Value) {
        let mut s = self.state.lock().unwrap();
        s.last = Some(message.clone());
        s.queued.push(message);
    }

    fn text(&self, from: &str, text: &str, context: Option<&str>) -> String {
        let id = format!("{}", 5_000 + self.next.fetch_add(1, Ordering::Relaxed));
        let mut message = json!({
            "message_id": id, "from_user_id": from, "to_user_id": ACCOUNT, "message_type": 1,
            "item_list": [{"type": 1, "text_item": {"text": text}}],
        });
        if let Some(token) = context {
            message["context_token"] = json!(token);
        }
        self.deliver(message);
        id
    }
}

impl MockPlatform for ILinkMock {
    fn dm(&self) -> (String, String) {
        (PEER.to_owned(), PEER.to_owned())
    }

    fn inject(&self, text: &str) -> String {
        self.text(PEER, text, Some("ctx-conformance"))
    }

    fn redeliver(&self) {
        let last = self.state.lock().unwrap().last.clone();
        if let Some(message) = last {
            self.state.lock().unwrap().queued.push(message);
        }
    }

    fn drop_connection(&self) {}

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

    /// A message with the caption and one CDN item sealed under the
    /// mock's key (a voice item without iLink's own transcript).
    fn inject_media(
        &self,
        kind: MediaKind,
        name: &str,
        bytes: &[u8],
        caption: &str,
    ) -> Option<String> {
        let (item_type, field) = match kind {
            MediaKind::Image => (2, "image_item"),
            MediaKind::Voice => (3, "voice_item"),
            MediaKind::Video => (5, "video_item"),
            _ => (4, "file_item"),
        };
        let param = {
            let mut s = self.state.lock().unwrap();
            let param = format!("in-{}", s.files.len() + 1);
            s.files.push((param.clone(), ecb_encrypt(&CDN_KEY, bytes)));
            param
        };
        let aes = base64::engine::general_purpose::STANDARD.encode(CDN_KEY);
        let id = format!("{}", 5_000 + self.next.fetch_add(1, Ordering::Relaxed));
        self.deliver(json!({
            "message_id": id, "from_user_id": PEER, "to_user_id": ACCOUNT, "message_type": 1,
            "context_token": "ctx-conformance",
            "item_list": [
                {"type": 1, "text_item": {"text": caption}},
                {"type": item_type, field: {"media": {"encrypt_query_param": param, "aes_key": aes}, "file_name": name}},
            ],
        }));
        Some(id)
    }

    fn files(&self) -> Vec<MockFile> {
        self.state.lock().unwrap().delivered.clone()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_weixin_adapter_passes_the_conformance_suite() {
    let platform = ILinkMock::start();
    let bridge = Bridge::start(&[platform.api.host()]);
    let (kit, _root) = platform.install(&bridge);
    let report = kit
        .run(
            kit.adapter().unwrap(),
            Arc::clone(&platform) as Arc<dyn MockPlatform>,
        )
        .await;
    catalog::assert_passed(&report);
}

/// Scenario: Context token survives restart.
#[tokio::test(flavor = "multi_thread")]
async fn a_send_after_a_restart_carries_the_stored_context_token() {
    let platform = ILinkMock::start();
    let bridge = Bridge::start(&[platform.api.host()]);
    let (kit, _root) = platform.install(&bridge);
    let adapter = kit.adapter().unwrap();
    let link = catalog::Link::new();
    adapter.connect(link.dyn_link()).await.unwrap();
    platform.text(PEER, "remind me later", Some("ctx-42"));
    assert_eq!(link.wait_for(1).await.len(), 1);
    adapter.disconnect().await;
    drop(adapter);
    // A new adapter (the daemon restarted): a cron message to the peer.
    let restarted = kit.adapter().unwrap();
    restarted
        .connect(catalog::Link::new().dyn_link())
        .await
        .unwrap();
    restarted
        .send(&Outgoing::text(PEER, None, "here is your reminder"))
        .await
        .unwrap();
    let sends = platform.state.lock().unwrap().sends.clone();
    assert_eq!(sends.last().unwrap().1.as_deref(), Some("ctx-42"));
    restarted.disconnect().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stale_token_is_dropped_once_and_media_is_ecb_both_ways() {
    let platform = ILinkMock::start();
    let key = [6u8; 16];
    platform
        .state
        .lock()
        .unwrap()
        .files
        .push(("img-param".into(), ecb_encrypt(&key, b"JPEGBYTES")));
    let bridge = Bridge::start(&[platform.api.host()]);
    let (kit, _root) = platform.install(&bridge);
    let adapter = kit.adapter().unwrap();
    let link = catalog::Link::new();
    adapter.connect(link.dyn_link()).await.unwrap();
    let aes = base64::engine::general_purpose::STANDARD.encode(key);
    platform.deliver(json!({
        "message_id": "img1", "from_user_id": PEER, "message_type": 1, "context_token": "old",
        "item_list": [{"type": 2, "image_item": {"media": {"encrypt_query_param": "img-param", "aes_key": aes}}}],
    }));
    let admitted = link.wait_for(1).await;
    let mut stream = adapter
        .open_media(&admitted[0].attachments[0])
        .await
        .unwrap();
    let mut bytes = Vec::new();
    while let Some(chunk) = futures_util::StreamExt::next(&mut stream.chunks).await {
        bytes.extend(chunk.unwrap());
    }
    assert_eq!(bytes, b"JPEGBYTES");
    // The stored token went stale: the send goes once more without it.
    platform.state.lock().unwrap().stale.push(PEER.to_owned());
    let dir = catalog::scratch();
    let path = dir.path().join("photo.png");
    std::fs::write(&path, b"PNG!").unwrap();
    let mut out = Outgoing::text(PEER, None, "a photo");
    out.attachments
        .push(OutboundAttachment::new(&path.display().to_string()));
    adapter.send(&out).await.unwrap();
    {
        let state = platform.state.lock().unwrap();
        assert!(state.sends.iter().all(|(_, token, _)| token.is_none()));
        assert_eq!(state.sends[0].2[0]["type"], 2);
        assert_eq!(
            state.sends[0].2[0]["image_item"]["media"]["encrypt_query_param"],
            "stored-1"
        );
        // What reached the CDN is the file under the key the item names.
        let named = state.sends[0].2[0]["image_item"]["media"]["aes_key"]
            .as_str()
            .unwrap()
            .to_owned();
        let hex = base64::engine::general_purpose::STANDARD
            .decode(named)
            .unwrap();
        let key: Vec<u8> = (0..16)
            .map(|i| {
                u8::from_str_radix(std::str::from_utf8(&hex[i * 2..i * 2 + 2]).unwrap(), 16)
                    .unwrap()
            })
            .collect();
        assert_eq!(
            state.uploads[0],
            ecb_encrypt(&key.try_into().unwrap(), b"PNG!")
        );
    }
    adapter.disconnect().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_expired_session_pauses_polling() {
    let platform = ILinkMock::start();
    let bridge = Bridge::start(&[platform.api.host()]);
    let (kit, _root) = platform.install(&bridge);
    let adapter = kit.adapter().unwrap();
    let link = catalog::Link::new();
    platform.state.lock().unwrap().expire_next = true;
    adapter.connect(link.dyn_link()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    platform.text(PEER, "anyone there?", None);
    // Paused for 10 minutes: nothing is fetched meanwhile.
    assert!(link.wait_for(1).await.is_empty());
    assert_eq!(platform.state.lock().unwrap().queued.len(), 1);
    adapter.disconnect().await;
}

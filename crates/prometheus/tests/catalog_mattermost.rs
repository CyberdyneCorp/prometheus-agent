//! The `mattermost` catalog adapter plugin (spec: platform-adapters,
//! Mattermost adapter; Catalog adapter set) against a mock Mattermost
//! serving REST API v4 and `/api/v4/websocket` on one port, the socket
//! held by the bridge sidecar (system Node): the conformance suite (files
//! both ways through the plugin host), channel gating with
//! `allowed_channels` first, thread replies and user-id allowlists.

#![cfg(feature = "gateway")]

mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use common::catalog::{self, Bridge, Reply, Request};
use common::catalog_ws::{Socket, WsServer};
use pmt_gateway::messaging::conformance::{
    Failure, MockFile, MockPlatform, MockSent, kind_of_name,
};
use pmt_gateway::messaging::{MediaKind, Outgoing};
use serde_json::{Value, json};

const TOKEN: &str = "mm-token";
const BOT: &str = "b0tb0tb0tb0tb0tb0tb0tb0tb0";
const USER: &str = "u5eru5eru5eru5eru5eru5eru5";
const DM: &str = "dmchannel";
const TOWN: &str = "town-square-id";
const OFF_TOPIC: &str = "off-topic-id";

#[derive(Default)]
struct State {
    queued: Vec<String>,
    last: Option<String>,
    posts: Vec<(MockSent, Vec<String>)>,
    /// Uploaded and inbound files: id -> (channel, name, bytes).
    files: Vec<(String, String, String, Vec<u8>)>,
    fail_next: Option<Failure>,
}

struct Mattermost {
    server: WsServer,
    state: Arc<Mutex<State>>,
    next: AtomicU64,
}

fn posted(
    id: &str,
    channel: &str,
    channel_type: &str,
    user: &str,
    text: &str,
    files: &[String],
    root: &str,
) -> String {
    let post = json!({
        "id": id, "channel_id": channel, "user_id": user, "message": text,
        "root_id": root, "file_ids": files, "type": "",
    });
    json!({"event": "posted", "data": {"channel_type": channel_type, "post": post.to_string(), "sender_name": "@kit"}})
        .to_string()
}

/// The file part of a multipart upload: its file name and bytes.
fn multipart_file(body: &[u8]) -> Option<(String, Vec<u8>)> {
    let text = String::from_utf8_lossy(body);
    let at = text.find("filename=\"")? + "filename=\"".len();
    let name = text[at..].split('"').next()?.to_owned();
    let start = body.windows(4).skip(at).position(|w| w == b"\r\n\r\n")? + at + 4;
    let end = body[start..].windows(4).position(|w| w == b"\r\n--")? + start;
    Some((name, body[start..end].to_vec()))
}

fn rest(state: &Mutex<State>, request: &Request) -> Reply {
    if request.header("authorization") != Some(&format!("Bearer {TOKEN}")) {
        return Reply::json(401, &json!({"id": "api.context.session_expired.app_error"}));
    }
    let mut state = state.lock().unwrap();
    let route = request.route().to_owned();
    match (request.method.as_str(), route.as_str()) {
        ("GET", "/api/v4/users/me") => Reply::json(200, &json!({"id": BOT, "username": "pbot"})),
        ("GET", path) if path.starts_with("/api/v4/channels/") => {
            let id = path.trim_start_matches("/api/v4/channels/");
            let kind = if id == DM { "D" } else { "O" };
            Reply::json(200, &json!({"id": id, "type": kind, "display_name": id}))
        }
        ("GET", path) if path.starts_with("/api/v4/files/") => {
            let rest = path.trim_start_matches("/api/v4/files/");
            let (id, info) = rest
                .strip_suffix("/info")
                .map_or((rest, false), |id| (id, true));
            match state.files.iter().find(|f| f.0 == id) {
                Some(file) if info => Reply::json(200, &json!({"id": id, "name": file.2})),
                Some(file) => Reply {
                    status: 200,
                    headers: Vec::new(),
                    body: file.3.clone(),
                },
                None => Reply::status(404),
            }
        }
        ("POST", "/api/v4/files") => {
            let Some((name, bytes)) = multipart_file(&request.body) else {
                return Reply::status(400);
            };
            let channel = String::from_utf8_lossy(&request.body)
                .split("name=\"channel_id\"\r\n\r\n")
                .nth(1)
                .and_then(|r| r.split("\r\n").next())
                .unwrap_or_default()
                .to_owned();
            let id = format!("up{}", state.files.len() + 1);
            state.files.push((id.clone(), channel, name, bytes));
            Reply::json(201, &json!({"file_infos": [{"id": id}]}))
        }
        ("POST", "/api/v4/posts") => {
            match state.fail_next.take() {
                Some(Failure::RateLimited(_)) => return Reply::json(429, &json!({})),
                Some(Failure::ServerError) => return Reply::status(500),
                Some(Failure::Forbidden | Failure::Markup) => return Reply::json(403, &json!({})),
                None => {}
            }
            let body = request.json();
            let message = body["message"].as_str().unwrap_or_default();
            if message.chars().count() > 16383 {
                return Reply::json(400, &json!({"id": "model.post.is_valid.msg.app_error"}));
            }
            let id = format!("p{}", state.posts.len() + 1);
            let files = body["file_ids"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|f| f.as_str().map(str::to_owned))
                .collect();
            let sent = MockSent {
                chat_id: body["channel_id"].as_str().unwrap_or_default().to_owned(),
                message_id: id.clone(),
                text: message.to_owned(),
                buttons: Vec::new(),
                thread_id: body["root_id"].as_str().map(str::to_owned),
            };
            state.posts.push((sent, files));
            Reply::json(201, &json!({"id": id}))
        }
        _ => Reply::status(404),
    }
}

impl Mattermost {
    async fn start() -> Arc<Self> {
        let state = Arc::new(Mutex::new(State::default()));
        let (on_open, on_rest) = (Arc::clone(&state), Arc::clone(&state));
        let server = WsServer::start(
            Arc::new(move |socket: &Socket| {
                socket.send(json!({"event": "hello", "data": {}}).to_string());
                for frame in on_open.lock().unwrap().queued.drain(..) {
                    socket.send(frame);
                }
                true
            }),
            Arc::new(|_: &Socket, _: &str| {}),
            Arc::new(move |request: &Request| rest(&on_rest, request)),
        )
        .await;
        Arc::new(Self {
            server,
            state,
            next: AtomicU64::new(1),
        })
    }

    fn options(&self, bridge: &Bridge) -> Value {
        json!({"server_url": self.server.base(), "sidecar": bridge.sidecar})
    }

    fn deliver(&self, frame: String) {
        self.state.lock().unwrap().last = Some(frame.clone());
        if !self.server.broadcast(&frame) {
            self.state.lock().unwrap().queued.push(frame);
        }
    }

    fn post(&self, channel: &str, kind: &str, text: &str, files: &[String]) -> String {
        let id = format!("in{}", self.next.fetch_add(1, Ordering::Relaxed));
        self.deliver(posted(&id, channel, kind, USER, text, files, ""));
        id
    }
}

impl MockPlatform for Mattermost {
    fn dm(&self) -> (String, String) {
        (DM.to_owned(), USER.to_owned())
    }

    fn inject(&self, text: &str) -> String {
        self.post(DM, "D", text, &[])
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
        self.state
            .lock()
            .unwrap()
            .posts
            .iter()
            .map(|(s, _)| s.clone())
            .collect()
    }

    fn typing(&self) -> usize {
        0
    }

    fn reactions(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    fn thread(&self) -> Option<(String, String)> {
        Some((TOWN.to_owned(), "root1".to_owned()))
    }

    fn inject_media(
        &self,
        _kind: MediaKind,
        name: &str,
        bytes: &[u8],
        caption: &str,
    ) -> Option<String> {
        let id = {
            let mut state = self.state.lock().unwrap();
            let id = format!("in-file{}", state.files.len() + 1);
            state
                .files
                .push((id.clone(), DM.to_owned(), name.to_owned(), bytes.to_vec()));
            id
        };
        Some(self.post(DM, "D", caption, &[id]))
    }

    fn files(&self) -> Vec<MockFile> {
        let state = self.state.lock().unwrap();
        state
            .posts
            .iter()
            .flat_map(|(sent, ids)| ids.iter().map(move |id| (sent, id)))
            .filter_map(|(sent, id)| {
                let file = state.files.iter().find(|f| &f.0 == id)?;
                Some(MockFile {
                    chat_id: sent.chat_id.clone(),
                    thread_id: sent.thread_id.clone(),
                    name: file.2.clone(),
                    kind: kind_of_name(&file.2),
                    bytes: file.3.clone(),
                })
            })
            .collect()
    }
}

fn install(
    mm: &Mattermost,
    bridge: &Bridge,
    extra: &Value,
) -> (
    prometheus::plugins_cmd::adapter_plugin::PluginKit,
    tempfile::TempDir,
) {
    let mut options = mm.options(bridge);
    for (k, v) in extra.as_object().into_iter().flatten() {
        options[k] = v.clone();
    }
    let (kit, root) = catalog::install(
        "mattermost",
        &options,
        &[
            mm.server.host(),
            bridge.base.trim_start_matches("http://").to_owned(),
        ],
    );
    catalog::secret(&kit, "MATTERMOST_TOKEN", TOKEN);
    (kit, root)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_mattermost_adapter_passes_the_conformance_suite() {
    let mm = Mattermost::start().await;
    let bridge = Bridge::start(&[mm.server.host()]);
    let (kit, _root) = install(&mm, &bridge, &json!({}));
    let report = kit
        .run(
            kit.adapter().unwrap(),
            Arc::clone(&mm) as Arc<dyn MockPlatform>,
        )
        .await;
    catalog::assert_passed(&report);
}

/// Scenario: Channel outside allowlist.
#[tokio::test(flavor = "multi_thread")]
async fn a_mention_outside_the_allowed_channels_is_ignored() {
    let mm = Mattermost::start().await;
    let bridge = Bridge::start(&[mm.server.host()]);
    let (kit, _root) = install(
        &mm,
        &bridge,
        &json!({"allowed_channels": [TOWN, OFF_TOPIC], "free_response_channels": [OFF_TOPIC], "reply_mode": "thread"}),
    );
    let adapter = kit.adapter().unwrap();
    let link = catalog::Link::new();
    adapter.connect(link.dyn_link()).await.unwrap();
    mm.post("elsewhere-id", "O", "@pbot are you there?", &[]);
    mm.post(TOWN, "O", "chatter nobody asked the bot about", &[]);
    let asked = mm.post(TOWN, "O", "@PBot status please", &[]);
    mm.post(OFF_TOPIC, "O", "free talk", &[]);
    let admitted = link.wait_for(2).await;
    let texts: Vec<&str> = admitted.iter().map(|i| i.text.as_str()).collect();
    assert_eq!(texts, ["status please", "free talk"], "{admitted:?}");
    assert!(admitted.iter().all(|i| i.addressing.mentioned));
    // In thread mode a top-level post roots the reply's thread.
    assert_eq!(admitted[0].thread_id.as_deref(), Some(asked.as_str()));
    adapter
        .send(&Outgoing::text(TOWN, Some(&asked), "on it"))
        .await
        .unwrap();
    let sent = mm.sent();
    assert_eq!(sent[0].thread_id.as_deref(), Some(asked.as_str()));
    adapter.disconnect().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_allowlist_of_usernames_is_refused() {
    let mm = Mattermost::start().await;
    let bridge = Bridge::start(&[mm.server.host()]);
    let (mut kit, _root) = install(&mm, &bridge, &json!({}));
    kit.allow_users(&["alice"]);
    let adapter = kit.adapter().unwrap();
    let err = adapter
        .connect(catalog::Link::new().dyn_link())
        .await
        .unwrap_err();
    assert!(
        err.message.contains("26-character user ids"),
        "{}",
        err.message
    );
}

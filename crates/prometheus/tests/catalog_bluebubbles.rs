//! The `bluebubbles` catalog adapter plugin (spec: platform-adapters,
//! `BlueBubbles` iMessage adapter; Catalog adapter set) against a mock
//! `BlueBubbles` server: the conformance suite (files both ways, tapbacks
//! and typing through the Private API), the webhook registered at connect
//! and removed at a clean stop, webhooks without the token refused, and
//! mention patterns gating groups but never DMs.

#![cfg(feature = "gateway")]

mod common;

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use common::catalog::{self, MockHttp, Reply, Request};
use pmt_gateway::messaging::MediaKind;
use pmt_gateway::messaging::conformance::{
    Failure, MockFile, MockPlatform, MockSent, kind_of_name,
};
use serde_json::{Value, json};

const PASSWORD: &str = "bb-password";
const USER: &str = "+15550001111";
const DM: &str = "iMessage;-;+15550001111";
const GROUP: &str = "iMessage;+;chat42";
const PUBLIC_URL: &str = "https://bb.example.com/p/default/bluebubbles";

#[derive(Default)]
struct State {
    adapter: Option<String>,
    webhooks: Vec<(u64, String, Vec<String>)>,
    deleted: Vec<u64>,
    sent: Vec<MockSent>,
    files_out: Vec<MockFile>,
    reactions: Vec<(String, String)>,
    typing: usize,
    read: Vec<String>,
    downloads: Vec<(String, Vec<u8>)>,
    last: Option<Value>,
    fail_next: Option<Failure>,
}

struct Server {
    api: MockHttp,
    state: Arc<Mutex<State>>,
    next: AtomicU64,
}

fn token() -> String {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, PASSWORD.as_bytes());
    ring::hmac::sign(&key, b"prometheus-bluebubbles-webhook")
        .as_ref()
        .iter()
        .fold(String::new(), |mut hex, b| {
            let _ = write!(hex, "{b:02x}");
            hex
        })
}

fn multipart_field(body: &[u8], name: &str) -> String {
    let text = String::from_utf8_lossy(body);
    text.split(&format!("name=\"{name}\"\r\n\r\n"))
        .nth(1)
        .and_then(|r| r.split("\r\n").next())
        .unwrap_or_default()
        .to_owned()
}

fn multipart_file(body: &[u8]) -> (String, Vec<u8>) {
    let text = String::from_utf8_lossy(body);
    let at = text.find("filename=\"").unwrap() + "filename=\"".len();
    let name = text[at..].split('"').next().unwrap().to_owned();
    let start = body
        .windows(4)
        .skip(at)
        .position(|w| w == b"\r\n\r\n")
        .unwrap()
        + at
        + 4;
    let end = body[start..]
        .windows(4)
        .position(|w| w == b"\r\n--")
        .unwrap()
        + start;
    (name, body[start..end].to_vec())
}

fn guid_of(path: &str, suffix: &str) -> String {
    let encoded = path
        .trim_start_matches("/api/v1/chat/")
        .trim_end_matches(suffix);
    url::form_urlencoded::parse(format!("g={encoded}").as_bytes())
        .next()
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default()
}

fn failure(state: &mut State) -> Option<Reply> {
    Some(match state.fail_next.take()? {
        Failure::RateLimited(_) => Reply::json(429, &json!({"status": 429})),
        Failure::ServerError => Reply::status(500),
        Failure::Forbidden | Failure::Markup => Reply::json(403, &json!({"status": 403})),
    })
}

/// The webhook registry: list, register, remove.
fn webhooks(state: &mut State, request: &Request, body: &Value) -> Reply {
    let ok = |data: Value| Reply::json(200, &json!({"status": 200, "data": data}));
    match (request.method.as_str(), request.route()) {
        ("GET", "/api/v1/webhook") => ok(json!(
            state
                .webhooks
                .iter()
                .map(|(id, url, _)| json!({"id": id, "url": url}))
                .collect::<Vec<_>>()
        )),
        ("POST", "/api/v1/webhook") => {
            let id = state.webhooks.len() as u64 + 1;
            let events = body["events"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|e| e.as_str().map(str::to_owned))
                .collect();
            state.webhooks.push((
                id,
                body["url"].as_str().unwrap_or_default().to_owned(),
                events,
            ));
            ok(json!({"id": id}))
        }
        ("DELETE", path) if path.starts_with("/api/v1/webhook/") => {
            let id = path
                .trim_start_matches("/api/v1/webhook/")
                .parse()
                .unwrap_or_default();
            state.deleted.push(id);
            state.webhooks.retain(|(w, ..)| *w != id);
            ok(json!({}))
        }
        _ => Reply::status(404),
    }
}

fn answer(state: &Mutex<State>, request: &Request) -> Reply {
    if !request.path.contains(&format!("password={PASSWORD}")) {
        return Reply::json(401, &json!({"status": 401}));
    }
    let mut state = state.lock().unwrap();
    let route = request.route().to_owned();
    let body = request.json();
    let ok = |data: Value| Reply::json(200, &json!({"status": 200, "data": data}));
    match (request.method.as_str(), route.as_str()) {
        ("GET", "/api/v1/server/info") => ok(json!({"private_api": true})),
        (_, path) if path.starts_with("/api/v1/webhook") => webhooks(&mut state, request, &body),
        ("POST", "/api/v1/chat/query") => ok(json!([
            {"guid": DM, "chatIdentifier": USER},
            {"guid": GROUP, "chatIdentifier": "chat42"},
        ])),
        ("POST", "/api/v1/message/text" | "/api/v1/chat/new") => {
            if let Some(reply) = failure(&mut state) {
                return reply;
            }
            let id = format!("sent{}", state.sent.len() + 1);
            let chat = body["chatGuid"]
                .as_str()
                .map_or_else(|| body["addresses"][0].to_string(), str::to_owned);
            state.sent.push(MockSent {
                chat_id: chat,
                message_id: id.clone(),
                text: body["message"].as_str().unwrap_or_default().to_owned(),
                buttons: Vec::new(),
                thread_id: None,
            });
            ok(json!({"guid": id}))
        }
        ("POST", "/api/v1/message/attachment") => {
            let (name, bytes) = multipart_file(&request.body);
            let chat = multipart_field(&request.body, "chatGuid");
            state.files_out.push(MockFile {
                chat_id: chat,
                thread_id: None,
                kind: kind_of_name(&name),
                name,
                bytes,
            });
            ok(json!({"guid": format!("file{}", state.files_out.len())}))
        }
        ("POST", "/api/v1/message/react") => {
            let tapback = body["reaction"].as_str().unwrap_or_default();
            let emoji = if tapback == "like" {
                "\u{1f440}"
            } else {
                tapback
            };
            state.reactions.push((
                body["selectedMessageGuid"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                emoji.to_owned(),
            ));
            ok(json!({}))
        }
        ("POST", path) if path.ends_with("/typing") => {
            state.typing += 1;
            ok(json!({}))
        }
        ("POST", path) if path.ends_with("/read") => {
            state.read.push(guid_of(path, "/read"));
            ok(json!({}))
        }
        ("GET", path) if path.starts_with("/api/v1/attachment/") => {
            let guid = path
                .trim_start_matches("/api/v1/attachment/")
                .trim_end_matches("/download");
            match state.downloads.iter().find(|(g, _)| g == guid) {
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

impl Server {
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

    fn options(&self) -> Value {
        json!({"server_url": self.api.base(), "public_url": PUBLIC_URL})
    }

    /// Posts a webhook event with `token` (the right one when `None`).
    fn post(&self, event: &Value, token_override: Option<&str>) -> u16 {
        let base = self
            .state
            .lock()
            .unwrap()
            .adapter
            .clone()
            .expect("the kit listens");
        self.state.lock().unwrap().last = Some(event.clone());
        let token = token_override.map_or_else(token, str::to_owned);
        catalog::post(
            &format!("{base}/?token={token}"),
            &[("content-type", "application/json")],
            event.to_string().as_bytes(),
        )
        .0
    }

    fn message(&self, chat: &str, text: &str, attachments: &Value) -> (String, Value) {
        let id = format!("in{}", self.next.fetch_add(1, Ordering::Relaxed));
        let event = json!({"type": "new-message", "data": {
            "guid": id, "text": text, "isFromMe": false, "handle": {"address": USER},
            "chats": [{"guid": chat}], "attachments": attachments,
        }});
        (id, event)
    }
}

impl MockPlatform for Server {
    fn dm(&self) -> (String, String) {
        (DM.to_owned(), USER.to_owned())
    }

    fn inject(&self, text: &str) -> String {
        let (id, event) = self.message(DM, text, &json!([]));
        assert_eq!(self.post(&event, None), 200);
        id
    }

    fn redeliver(&self) {
        let last = self.state.lock().unwrap().last.clone();
        if let Some(event) = last {
            self.post(&event, None);
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
        self.state.lock().unwrap().typing
    }

    fn reactions(&self) -> Vec<(String, String)> {
        self.state.lock().unwrap().reactions.clone()
    }

    fn listen(&self, base: &str) {
        self.state.lock().unwrap().adapter = Some(base.to_owned());
    }

    fn inject_media(
        &self,
        _kind: MediaKind,
        name: &str,
        bytes: &[u8],
        caption: &str,
    ) -> Option<String> {
        let guid = format!("att{}", self.next.fetch_add(1, Ordering::Relaxed));
        self.state
            .lock()
            .unwrap()
            .downloads
            .push((guid.clone(), bytes.to_vec()));
        let (id, event) = self.message(DM, caption, &json!([{"guid": guid, "transferName": name}]));
        assert_eq!(self.post(&event, None), 200);
        Some(id)
    }

    fn files(&self) -> Vec<MockFile> {
        self.state.lock().unwrap().files_out.clone()
    }

    fn voice_notes_out(&self) -> bool {
        false
    }
}

fn install(
    server: &Server,
    extra: &Value,
) -> (
    prometheus::plugins_cmd::adapter_plugin::PluginKit,
    tempfile::TempDir,
) {
    let mut options = server.options();
    for (k, v) in extra.as_object().into_iter().flatten() {
        options[k] = v.clone();
    }
    let (kit, root) = catalog::install("bluebubbles", &options, &[server.api.host()]);
    catalog::secret(&kit, "BLUEBUBBLES_PASSWORD", PASSWORD);
    (kit, root)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_bluebubbles_adapter_passes_the_conformance_suite() {
    let server = Server::start();
    let (kit, _root) = install(&server, &json!({}));
    let report = kit
        .run(
            kit.adapter().unwrap(),
            Arc::clone(&server) as Arc<dyn MockPlatform>,
        )
        .await;
    catalog::assert_passed(&report);
}

/// Scenario: Webhook registered.
#[tokio::test(flavor = "multi_thread")]
async fn connecting_registers_the_webhook_and_a_clean_stop_removes_it() {
    let server = Server::start();
    let (kit, _root) = install(
        &server,
        &json!({"require_mention": true, "mention_patterns": ["@pbot"]}),
    );
    let adapter = kit.adapter().unwrap();
    let link = catalog::Link::new();
    let prefix = catalog::serve(Arc::clone(&adapter)).await;
    server.listen(&prefix);
    adapter.connect(link.dyn_link()).await.unwrap();
    let webhooks = server.state.lock().unwrap().webhooks.clone();
    assert_eq!(webhooks.len(), 1);
    assert_eq!(webhooks[0].1, format!("{PUBLIC_URL}/?token={}", token()));
    assert_eq!(webhooks[0].2, ["new-message", "updated-message"]);
    // Without the token nothing is admitted.
    let (_, forged) = server.message(DM, "forged", &json!([]));
    assert_eq!(server.post(&forged, Some("nope")), 401);
    // Groups answer only to the mention patterns; DMs always.
    let (_, chatter) = server.message(GROUP, "just chatting", &json!([]));
    let (_, asked) = server.message(GROUP, "hey @PBot help", &json!([]));
    let (_, direct) = server.message(DM, "direct question", &json!([]));
    for event in [&chatter, &asked, &direct] {
        assert_eq!(server.post(event, None), 200);
    }
    let admitted = link.wait_for(2).await;
    let texts: Vec<&str> = admitted.iter().map(|i| i.text.as_str()).collect();
    assert_eq!(texts, ["hey @PBot help", "direct question"]);
    // Read receipts go through the Private API.
    assert!(server.state.lock().unwrap().read.contains(&DM.to_owned()));
    adapter.disconnect().await;
    let state = server.state.lock().unwrap();
    assert_eq!(state.deleted, [1]);
    assert!(state.webhooks.is_empty());
}

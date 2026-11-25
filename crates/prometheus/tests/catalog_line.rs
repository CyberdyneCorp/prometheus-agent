//! The `line` catalog adapter plugin (spec: platform-adapters, LINE
//! adapter; Catalog adapter set) against a mock Messaging API: the
//! conformance suite (media in downloaded by the plugin, media out served
//! from tokenized URLs on the adapter's own route), signed webhooks, the
//! reply token before Push, and the slow-turn "Still thinking" button
//! whose press delivers the held answer without a Push.

#![cfg(feature = "gateway")]

mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use common::catalog::{self, MockHttp, Reply, Request};
use pmt_gateway::messaging::adapter::OutboundAttachment;
use pmt_gateway::messaging::conformance::{
    Failure, MockFile, MockPlatform, MockSent, kind_of_name,
};
use pmt_gateway::messaging::{MediaKind, Outgoing};
use serde_json::{Value, json};

const SECRET: &str = "line-channel-secret";
const TOKEN: &str = "line-access-token";
const USER: &str = "Ukituser";
const PUBLIC_URL: &str = "https://line.example.com/p/default/line";

#[derive(Default)]
struct State {
    adapter: Option<String>,
    /// Calls: ("reply" | "push", chat or token, messages).
    calls: Vec<(String, String, Vec<Value>)>,
    /// Reply tokens handed out, and whether each was used: token -> chat.
    tokens: Vec<(String, String, bool)>,
    contents: Vec<(String, Vec<u8>)>,
    last: Option<Value>,
    fail_next: Option<Failure>,
}

struct Line {
    api: MockHttp,
    state: Arc<Mutex<State>>,
    next: AtomicU64,
}

fn signature(body: &[u8]) -> String {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, SECRET.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(ring::hmac::sign(&key, body).as_ref())
}

fn answer(state: &Mutex<State>, request: &Request) -> Reply {
    if request.header("authorization") != Some(&format!("Bearer {TOKEN}")) {
        return Reply::json(401, &json!({"message": "Authentication failed"}));
    }
    let mut state = state.lock().unwrap();
    let route = request.route().to_owned();
    if let Some(id) = route
        .strip_prefix("/v2/bot/message/")
        .and_then(|r| r.strip_suffix("/content"))
    {
        return match state.contents.iter().find(|(i, _)| i == id) {
            Some((_, bytes)) => Reply {
                status: 200,
                headers: Vec::new(),
                body: bytes.clone(),
            },
            None => Reply::status(404),
        };
    }
    match state.fail_next.take() {
        Some(Failure::RateLimited(_)) => return Reply::json(429, &json!({"message": "rate"})),
        Some(Failure::ServerError) => return Reply::status(500),
        Some(Failure::Forbidden | Failure::Markup) => {
            return Reply::json(403, &json!({"message": "no"}));
        }
        None => {}
    }
    let body = request.json();
    let messages = body["messages"].as_array().cloned().unwrap_or_default();
    if messages.len() > 5
        || messages
            .iter()
            .any(|m| m["text"].as_str().is_some_and(|t| t.chars().count() > 5000))
    {
        return Reply::json(400, &json!({"message": "The request body has 1 error(s)"}));
    }
    match route.as_str() {
        "/v2/bot/message/reply" => {
            let token = body["replyToken"].as_str().unwrap_or_default();
            let Some(entry) = state
                .tokens
                .iter_mut()
                .find(|(t, _, used)| t == token && !used)
            else {
                return Reply::json(400, &json!({"message": "Invalid reply token"}));
            };
            entry.2 = true;
            let chat = entry.1.clone();
            state.calls.push(("reply".into(), chat, messages));
            Reply::json(200, &json!({}))
        }
        "/v2/bot/message/push" => {
            let to = body["to"].as_str().unwrap_or_default().to_owned();
            state.calls.push(("push".into(), to, messages));
            Reply::json(200, &json!({}))
        }
        _ => Reply::status(404),
    }
}

impl Line {
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

    fn options(&self, slow_secs: u64) -> Value {
        json!({
            "public_url": PUBLIC_URL,
            "api_base": self.api.base(),
            "data_api_base": self.api.base(),
            "slow_response_secs": slow_secs,
            "allowed_users": [USER],
        })
    }

    /// Posts a webhook carrying `event` (with a fresh reply token):
    /// the adapter's status.
    fn webhook(&self, mut event: Value, signature_override: Option<&str>) -> u16 {
        let token = format!("rt{}", self.next.fetch_add(1, Ordering::Relaxed));
        event["replyToken"] = json!(token);
        event["source"] = json!({"type": "user", "userId": USER});
        let mut state = self.state.lock().unwrap();
        state.tokens.push((token, USER.to_owned(), false));
        state.last = Some(event.clone());
        let base = state.adapter.clone().expect("the kit listens");
        drop(state);
        let body = json!({"destination": "Ubot", "events": [event]}).to_string();
        let right = signature(body.as_bytes());
        catalog::post(
            &format!("{base}/"),
            &[
                ("content-type", "application/json"),
                ("x-line-signature", signature_override.unwrap_or(&right)),
            ],
            body.as_bytes(),
        )
        .0
    }

    fn text(&self, text: &str) -> String {
        let id = format!("m{}", self.next.fetch_add(1, Ordering::Relaxed));
        let event = json!({"type": "message", "webhookEventId": id, "message": {"id": id, "type": "text", "text": text}});
        assert_eq!(self.webhook(event, None), 200);
        id
    }

    fn calls(&self) -> Vec<(String, String, Vec<Value>)> {
        self.state.lock().unwrap().calls.clone()
    }

    /// Fetches a sent file from the adapter's route, as LINE would.
    fn fetch(&self, url: &str) -> Vec<u8> {
        let path = url.strip_prefix(PUBLIC_URL).unwrap();
        let base = self.state.lock().unwrap().adapter.clone().unwrap();
        let (status, body) = catalog::request("GET", &format!("{base}{path}"), &[], b"");
        assert_eq!(status, 200, "{url}");
        body
    }
}

impl MockPlatform for Line {
    fn dm(&self) -> (String, String) {
        (USER.to_owned(), USER.to_owned())
    }

    fn inject(&self, text: &str) -> String {
        self.text(text)
    }

    fn redeliver(&self) {
        let last = self.state.lock().unwrap().last.clone();
        if let Some(event) = last {
            self.webhook(event, None);
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
        self.calls()
            .iter()
            .flat_map(|(_, chat, messages)| messages.iter().map(move |m| (chat.clone(), m.clone())))
            .filter_map(|(chat, m)| {
                Some(MockSent {
                    chat_id: chat,
                    message_id: String::new(),
                    text: m["text"].as_str()?.to_owned(),
                    buttons: Vec::new(),
                    thread_id: None,
                })
            })
            .collect()
    }

    fn typing(&self) -> usize {
        0
    }

    fn reactions(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    fn listen(&self, base: &str) {
        self.state.lock().unwrap().adapter = Some(base.to_owned());
    }

    fn inject_media(
        &self,
        kind: MediaKind,
        name: &str,
        bytes: &[u8],
        caption: &str,
    ) -> Option<String> {
        // LINE carries no caption with a file: the caption is a text
        // message of its own, which the kit does not tell apart; send the
        // file with the caption as the text the kit looks for.
        let id = format!("f{}", self.next.fetch_add(1, Ordering::Relaxed));
        self.state
            .lock()
            .unwrap()
            .contents
            .push((id.clone(), bytes.to_vec()));
        let kind = match kind {
            MediaKind::Image => "image",
            MediaKind::Video => "video",
            MediaKind::Audio | MediaKind::Voice => "audio",
            _ => "file",
        };
        let event = json!({"type": "message", "message": {"id": id, "type": kind, "fileName": name, "text": caption}});
        assert_eq!(self.webhook(event, None), 200);
        Some(id)
    }

    fn files(&self) -> Vec<MockFile> {
        let mut files = Vec::new();
        for (_, chat, messages) in self.calls() {
            for m in messages {
                let Some(url) = m["originalContentUrl"].as_str() else {
                    continue;
                };
                let name = url.rsplit('/').next().unwrap_or_default().to_owned();
                files.push(MockFile {
                    chat_id: chat.clone(),
                    thread_id: None,
                    kind: kind_of_name(&name),
                    bytes: self.fetch(url),
                    name,
                });
            }
        }
        files
    }

    fn voice_notes_out(&self) -> bool {
        false
    }
}

fn install(
    mock: &Line,
    slow_secs: u64,
) -> (
    prometheus::plugins_cmd::adapter_plugin::PluginKit,
    tempfile::TempDir,
) {
    let (kit, root) = catalog::install("line", &mock.options(slow_secs), &[mock.api.host()]);
    catalog::secret(&kit, "LINE_CHANNEL_SECRET", SECRET);
    catalog::secret(&kit, "LINE_CHANNEL_ACCESS_TOKEN", TOKEN);
    (kit, root)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_line_adapter_passes_the_conformance_suite() {
    let mock = Line::start();
    let (kit, _root) = install(&mock, 0);
    let report = kit
        .run(
            kit.adapter().unwrap(),
            Arc::clone(&mock) as Arc<dyn MockPlatform>,
        )
        .await;
    catalog::assert_passed(&report);
}

/// Scenario: Slow answer retrieved.
#[tokio::test(flavor = "multi_thread")]
async fn a_slow_answer_waits_for_the_button_and_costs_no_push() {
    let mock = Line::start();
    let (kit, _root) = install(&mock, 1);
    let adapter = kit.adapter().unwrap();
    let link = catalog::Link::new();
    let prefix = catalog::serve(Arc::clone(&adapter)).await;
    mock.listen(&prefix);
    adapter.connect(link.dyn_link()).await.unwrap();
    // A forged webhook is refused.
    let forged = json!({"type": "message", "message": {"id": "x", "type": "text", "text": "hi"}});
    assert_eq!(mock.webhook(forged, Some("Zm9yZ2Vk")), 401);
    mock.text("think hard");
    assert_eq!(link.wait_for(1).await.len(), 1);
    // The turn passes the threshold: the reply token buys the button.
    let mut button = None;
    for _ in 0..100 {
        button = mock
            .calls()
            .into_iter()
            .find(|(kind, _, m)| kind == "reply" && m[0]["type"] == "template");
        if button.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let (_, chat, messages) = button.expect("no Still thinking button");
    assert_eq!(chat, USER);
    assert_eq!(messages[0]["template"]["actions"][0]["label"], "Get answer");
    // The answer is held, not pushed.
    adapter
        .send(&Outgoing::text(USER, None, "the answer"))
        .await
        .unwrap();
    assert!(
        mock.calls().iter().all(|(kind, _, _)| kind == "reply"),
        "{:?}",
        mock.calls()
    );
    // The press brings a fresh token that delivers it.
    let data = messages[0]["template"]["actions"][0]["data"].clone();
    let press = json!({"type": "postback", "postback": {"data": data}});
    assert_eq!(mock.webhook(press, None), 200);
    let calls = mock.calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(calls[1].0, "reply");
    assert_eq!(calls[1].2[0]["text"], "the answer");
    assert!(calls.iter().all(|(kind, _, _)| kind != "push"));
    // The press itself starts no turn.
    assert_eq!(link.wait_for(2).await.len(), 1);
    adapter.disconnect().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stale_reply_token_falls_back_to_push_and_allowlists_check_id_kinds() {
    let mock = Line::start();
    let (kit, _root) = install(&mock, 0);
    let adapter = kit.adapter().unwrap();
    let prefix = catalog::serve(Arc::clone(&adapter)).await;
    mock.listen(&prefix);
    adapter
        .connect(catalog::Link::new().dyn_link())
        .await
        .unwrap();
    mock.text("hello");
    tokio::time::sleep(Duration::from_millis(600)).await;
    adapter
        .send(&Outgoing::text(USER, None, "first"))
        .await
        .unwrap();
    adapter
        .send(&Outgoing::text(USER, None, "second"))
        .await
        .unwrap();
    let kinds: Vec<String> = mock.calls().into_iter().map(|(k, _, _)| k).collect();
    assert_eq!(kinds, ["reply", "push"]);
    adapter.disconnect().await;

    let mut options = mock.options(0);
    options["allowed_groups"] = json!(["Ugroup"]);
    let (kit, _root) = catalog::install("line", &options, &[mock.api.host()]);
    catalog::secret(&kit, "LINE_CHANNEL_SECRET", SECRET);
    catalog::secret(&kit, "LINE_CHANNEL_ACCESS_TOKEN", TOKEN);
    let err = kit
        .adapter()
        .unwrap()
        .connect(catalog::Link::new().dyn_link())
        .await
        .unwrap_err();
    assert!(err.message.contains("starting with C"), "{}", err.message);
}

/// The URLs of the files sent so far.
fn media_urls(mock: &Line) -> Vec<String> {
    mock.calls()
        .into_iter()
        .flat_map(|(_, _, messages)| messages)
        .filter_map(|m| m["originalContentUrl"].as_str().map(str::to_owned))
        .collect()
}

/// A served file's link cannot be guessed: it is keyed with the channel
/// secret and differs after a restart even for the same file at the same
/// position, and a token made of the sequence number and name alone
/// (what an outsider could count through) finds nothing.
#[tokio::test(flavor = "multi_thread")]
async fn served_media_links_cannot_be_guessed() {
    let mock = Line::start();
    let (kit, _root) = install(&mock, 0);
    let adapter = kit.adapter().unwrap();
    let prefix = catalog::serve(Arc::clone(&adapter)).await;
    mock.listen(&prefix);
    let dir = catalog::scratch();
    let path = dir.path().join("image.png");
    std::fs::write(&path, b"PNG bytes").unwrap();
    let mut out = Outgoing::text(USER, None, "here");
    out.attachments
        .push(OutboundAttachment::new(&path.display().to_string()));
    for _ in 0..2 {
        adapter
            .connect(catalog::Link::new().dyn_link())
            .await
            .unwrap();
        adapter.send(&out).await.unwrap();
        adapter.disconnect().await;
    }
    let urls = media_urls(&mock);
    assert_eq!(urls.len(), 2, "{urls:?}");
    assert_ne!(urls[0], urls[1], "a restart repeated the link");
    adapter
        .connect(catalog::Link::new().dyn_link())
        .await
        .unwrap();
    adapter.send(&out).await.unwrap();
    let url = media_urls(&mock).pop().unwrap();
    assert_eq!(mock.fetch(&url), b"PNG bytes");
    let base = mock.state.lock().unwrap().adapter.clone().unwrap();
    let get = |path: &str| catalog::request("GET", &format!("{base}{path}"), &[], b"").0;
    let counted = ring::digest::digest(&ring::digest::SHA256, b"1:image.png");
    let guess = counted.as_ref()[..12]
        .iter()
        .fold(String::new(), |mut out, b| {
            use std::fmt::Write as _;
            let _ = write!(out, "{b:02x}");
            out
        });
    assert_eq!(get(&format!("/media/{guess}/image.png")), 404);
    adapter.disconnect().await;
}

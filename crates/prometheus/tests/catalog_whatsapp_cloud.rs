//! The `whatsapp_cloud` catalog adapter plugin (spec: platform-adapters,
//! `WhatsApp` Cloud API adapter; Catalog adapter set) against a mock Graph
//! API: the conformance suite (media both ways through `/media`, choices
//! as interactive messages, approvals as Approve/Deny with numbered
//! answers), the verify-token handshake, signed notifications, choice
//! rendering limits, and the closed 24-hour window with and without a
//! re-engagement template.

#![cfg(feature = "gateway")]

mod common;

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use common::catalog::{self, MockHttp, Reply, Request};
use pmt_gateway::messaging::conformance::{Failure, MockFile, MockPlatform, MockSent};
use pmt_gateway::messaging::{Button, MediaKind, Outgoing, SendError};
use serde_json::{Value, json};

const APP_SECRET: &str = "wa-app-secret";
const TOKEN: &str = "wa-access-token";
const VERIFY: &str = "wa-verify-token";
const PHONE_ID: &str = "106540352242922";
const USER: &str = "15551234567";

#[derive(Default)]
struct State {
    adapter: Option<String>,
    /// Every message payload accepted, with its id.
    messages: Vec<(String, Value)>,
    /// Uploaded media: (id, name, mime, bytes).
    uploads: Vec<(String, String, String, Vec<u8>)>,
    /// Media the platform holds for inbound messages: (id, bytes).
    inbound_media: Vec<(String, Vec<u8>)>,
    typing: usize,
    reads: Vec<String>,
    last: Option<Vec<u8>>,
    fail_next: Option<Failure>,
    /// Users outside the 24-hour window.
    closed: Vec<String>,
}

struct Graph {
    api: MockHttp,
    state: Arc<Mutex<State>>,
    next: AtomicU64,
}

fn graph_error(status: u16, code: i64) -> Reply {
    Reply::json(
        status,
        &json!({"error": {"message": "refused", "type": "OAuthException", "code": code}}),
    )
}

fn messages(state: &mut State, body: Value) -> Reply {
    if body["status"] == "read" {
        state
            .reads
            .push(body["message_id"].as_str().unwrap_or_default().to_owned());
        if body["typing_indicator"]["type"] == "text" {
            state.typing += 1;
        }
        return Reply::json(200, &json!({"success": true}));
    }
    let to = body["to"].as_str().unwrap_or_default().to_owned();
    if state.closed.contains(&to) && body["type"] != "template" {
        return graph_error(400, 131_047);
    }
    match state.fail_next.take() {
        Some(Failure::RateLimited(_)) => return graph_error(400, 130_429),
        Some(Failure::ServerError) => return Reply::status(500),
        Some(Failure::Forbidden | Failure::Markup) => return graph_error(400, 131_026),
        None => {}
    }
    let id = format!("wamid.out{}", state.messages.len() + 1);
    state.messages.push((id.clone(), body));
    Reply::json(200, &json!({"messages": [{"id": id}]}))
}

fn answer(base: &str, state: &Mutex<State>, request: &Request) -> Reply {
    if request.header("authorization") != Some(&format!("Bearer {TOKEN}")) {
        return graph_error(401, 190);
    }
    let mut state = state.lock().unwrap();
    let route = request.route().to_owned();
    if route == format!("/v20.0/{PHONE_ID}/messages") {
        return messages(&mut state, request.json());
    }
    if route == format!("/v20.0/{PHONE_ID}/media") {
        let (name, mime, bytes) = catalog::multipart_file(&request.body).unwrap();
        let id = format!("up{}", state.uploads.len() + 1);
        state.uploads.push((id.clone(), name, mime, bytes));
        return Reply::json(200, &json!({"id": id}));
    }
    if let Some(id) = route.strip_prefix("/download/") {
        return match state.inbound_media.iter().find(|(i, _)| i == id) {
            Some((_, bytes)) => Reply {
                status: 200,
                headers: Vec::new(),
                body: bytes.clone(),
            },
            None => Reply::status(404),
        };
    }
    if let Some(id) = route.strip_prefix("/v20.0/")
        && state.inbound_media.iter().any(|(i, _)| i == id)
    {
        return Reply::json(200, &json!({"url": format!("{base}/download/{id}")}));
    }
    Reply::status(404)
}

fn signature(body: &[u8]) -> String {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, APP_SECRET.as_bytes());
    let tag = ring::hmac::sign(&key, body);
    let hex = tag.as_ref().iter().fold(String::new(), |mut hex, b| {
        let _ = write!(hex, "{b:02x}");
        hex
    });
    format!("sha256={hex}")
}

impl Graph {
    fn start() -> Arc<Self> {
        let state = Arc::new(Mutex::new(State::default()));
        let served = Arc::clone(&state);
        let base = Arc::new(Mutex::new(String::new()));
        let seen = Arc::clone(&base);
        let api = MockHttp::start(Arc::new(move |request: &Request| {
            let base = seen.lock().unwrap().clone();
            answer(&base, &served, request)
        }));
        api.base().clone_into(&mut base.lock().unwrap());
        Arc::new(Self {
            api,
            state,
            next: AtomicU64::new(1),
        })
    }

    fn options(&self) -> Value {
        json!({"phone_number_id": PHONE_ID, "api_base": self.api.base()})
    }

    fn host(&self) -> String {
        self.api.host()
    }

    fn prefix(&self) -> String {
        self.state
            .lock()
            .unwrap()
            .adapter
            .clone()
            .expect("the kit listens")
    }

    /// Posts a notification body: the adapter's status.
    fn notify(&self, body: &[u8], signature_override: Option<&str>) -> u16 {
        let right = signature(body);
        catalog::post(
            &format!("{}/", self.prefix()),
            &[
                ("content-type", "application/json"),
                ("x-hub-signature-256", signature_override.unwrap_or(&right)),
            ],
            body,
        )
        .0
    }

    /// Delivers `message` (with `from` and `id` filled in) from the user.
    fn deliver(&self, mut message: Value, phone_id: &str) -> String {
        let id = format!("wamid.in{}", self.next.fetch_add(1, Ordering::Relaxed));
        message["from"] = json!(USER);
        message["id"] = json!(id);
        let body = json!({"object": "whatsapp_business_account", "entry": [{"id": "waba", "changes": [{"field": "messages", "value": {
            "messaging_product": "whatsapp",
            "metadata": {"display_phone_number": "15550000000", "phone_number_id": phone_id},
            "contacts": [{"profile": {"name": "Kit User"}, "wa_id": USER}],
            "messages": [message],
        }}]}]})
        .to_string()
        .into_bytes();
        self.state.lock().unwrap().last = Some(body.clone());
        assert_eq!(self.notify(&body, None), 200);
        id
    }

    fn payloads(&self) -> Vec<(String, Value)> {
        self.state.lock().unwrap().messages.clone()
    }

    fn close_window(&self, user: &str) {
        self.state.lock().unwrap().closed.push(user.to_owned());
    }
}

fn sent_of(id: &str, body: &Value) -> Option<MockSent> {
    let chat = body["to"].as_str()?.to_owned();
    let (text, buttons) = match body["type"].as_str()? {
        "text" => (body["text"]["body"].as_str()?.to_owned(), Vec::new()),
        "interactive" => {
            let inner = &body["interactive"];
            let buttons = inner["action"]["buttons"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|b| (&b["reply"]["title"], &b["reply"]["id"]))
                .chain(
                    inner["action"]["sections"][0]["rows"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|r| (&r["title"], &r["id"])),
                )
                .map(|(label, data)| Button {
                    label: label.as_str().unwrap_or_default().to_owned(),
                    data: data.as_str().unwrap_or_default().to_owned(),
                })
                .collect();
            (inner["body"]["text"].as_str()?.to_owned(), buttons)
        }
        _ => return None,
    };
    Some(MockSent {
        chat_id: chat,
        message_id: id.to_owned(),
        text,
        buttons,
        thread_id: None,
    })
}

impl MockPlatform for Graph {
    fn dm(&self) -> (String, String) {
        (USER.to_owned(), USER.to_owned())
    }

    fn inject(&self, text: &str) -> String {
        self.deliver(json!({"type": "text", "text": {"body": text}}), PHONE_ID)
    }

    fn redeliver(&self) {
        let last = self.state.lock().unwrap().last.clone();
        if let Some(body) = last {
            self.notify(&body, None);
        }
    }

    fn drop_connection(&self) {}

    fn press(&self, _message_id: &str, data: &str) {
        let reply =
            json!({"type": "button_reply", "button_reply": {"id": data, "title": "pressed"}});
        self.deliver(
            json!({"type": "interactive", "interactive": reply}),
            PHONE_ID,
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
        self.payloads()
            .iter()
            .filter_map(|(id, body)| sent_of(id, body))
            .collect()
    }

    fn typing(&self) -> usize {
        self.state.lock().unwrap().typing
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
        let media = format!("media{}", self.next.fetch_add(1, Ordering::Relaxed));
        self.state
            .lock()
            .unwrap()
            .inbound_media
            .push((media.clone(), bytes.to_vec()));
        let (kind, mut block) = match kind {
            MediaKind::Image => ("image", json!({"mime_type": "image/png"})),
            MediaKind::Voice => ("audio", json!({"mime_type": "audio/ogg", "voice": true})),
            MediaKind::Audio => ("audio", json!({"mime_type": "audio/mpeg"})),
            MediaKind::Video => ("video", json!({"mime_type": "video/mp4"})),
            _ => (
                "document",
                json!({"mime_type": "application/pdf", "filename": name}),
            ),
        };
        block["id"] = json!(media);
        // Real audio carries no caption; the mock adds one so the kit can
        // find the message.
        block["caption"] = json!(caption);
        let mut message = json!({"type": kind});
        message[kind] = block;
        Some(self.deliver(message, PHONE_ID))
    }

    fn files(&self) -> Vec<MockFile> {
        let state = self.state.lock().unwrap();
        let mut files = Vec::new();
        for (_, body) in &state.messages {
            let kind = body["type"].as_str().unwrap_or_default();
            let Some(upload) = body[kind]["id"]
                .as_str()
                .and_then(|id| state.uploads.iter().find(|u| u.0 == id))
            else {
                continue;
            };
            let shown = match kind {
                "image" => MediaKind::Image,
                "video" => MediaKind::Video,
                "audio" if upload.2.contains("codecs=opus") => MediaKind::Voice,
                "audio" => MediaKind::Audio,
                _ => MediaKind::Document,
            };
            files.push(MockFile {
                chat_id: body["to"].as_str().unwrap_or_default().to_owned(),
                thread_id: None,
                name: upload.1.clone(),
                kind: shown,
                bytes: upload.3.clone(),
            });
        }
        files
    }
}

fn install(
    mock: &Graph,
    options: &Value,
) -> (
    prometheus::plugins_cmd::adapter_plugin::PluginKit,
    tempfile::TempDir,
) {
    let (kit, root) = catalog::install("whatsapp_cloud", options, &[mock.host()]);
    catalog::secret(&kit, "WHATSAPP_CLOUD_ACCESS_TOKEN", TOKEN);
    catalog::secret(&kit, "WHATSAPP_CLOUD_APP_SECRET", APP_SECRET);
    catalog::secret(&kit, "WHATSAPP_CLOUD_VERIFY_TOKEN", VERIFY);
    (kit, root)
}

async fn connected(
    mock: &Arc<Graph>,
    options: &Value,
) -> (
    Arc<dyn pmt_gateway::messaging::Adapter>,
    Arc<catalog::Link>,
    tempfile::TempDir,
) {
    let (kit, root) = install(mock, options);
    let adapter = kit.adapter().unwrap();
    let prefix = catalog::serve(Arc::clone(&adapter)).await;
    mock.listen(&prefix);
    let link = catalog::Link::new();
    adapter.connect(link.dyn_link()).await.unwrap();
    (adapter, link, root)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_whatsapp_cloud_adapter_passes_the_conformance_suite() {
    let mock = Graph::start();
    let (kit, _root) = install(&mock, &mock.options());
    let report = kit
        .run(
            kit.adapter().unwrap(),
            Arc::clone(&mock) as Arc<dyn MockPlatform>,
        )
        .await;
    catalog::assert_passed(&report);
}

/// Scenario: Wrong verify token.
#[tokio::test(flavor = "multi_thread")]
async fn a_handshake_with_the_wrong_verify_token_is_refused() {
    let mock = Graph::start();
    let (adapter, _link, _root) = connected(&mock, &mock.options()).await;
    let handshake = |token: &str| {
        catalog::request(
            "GET",
            &format!(
                "{}/?hub.mode=subscribe&hub.verify_token={token}&hub.challenge=1158201444",
                mock.prefix()
            ),
            &[],
            b"",
        )
    };
    let (status, body) = handshake("not-the-token");
    assert_eq!(status, 403);
    assert!(!String::from_utf8_lossy(&body).contains("1158201444"));
    let (status, body) = handshake(VERIFY);
    assert_eq!((status, body.as_slice()), (200, b"1158201444".as_slice()));
    adapter.disconnect().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn notifications_must_be_signed_and_for_this_number() {
    let mock = Graph::start();
    let (adapter, link, _root) = connected(&mock, &mock.options()).await;
    let body = br#"{"entry":[]}"#;
    assert_eq!(mock.notify(body, Some("sha256=00")), 401);
    assert_eq!(mock.notify(body, None), 200);
    mock.deliver(
        json!({"type": "text", "text": {"body": "for another number"}}),
        "999999999999999",
    );
    let id = mock.inject("for us");
    let got = link.wait_for(1).await;
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].text, "for us");
    assert_eq!(got[0].sender_name.as_deref(), Some("Kit User"));
    // Read with a typing indicator.
    assert!(mock.state.lock().unwrap().reads.contains(&id));
    adapter.disconnect().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_app_secret_missing_is_503() {
    let mock = Graph::start();
    let (kit, _root) = catalog::install("whatsapp_cloud", &mock.options(), &[mock.host()]);
    catalog::secret(&kit, "WHATSAPP_CLOUD_ACCESS_TOKEN", TOKEN);
    let adapter = kit.adapter().unwrap();
    let prefix = catalog::serve(Arc::clone(&adapter)).await;
    mock.listen(&prefix);
    adapter
        .connect(catalog::Link::new().dyn_link())
        .await
        .unwrap();
    assert_eq!(mock.notify(b"{}", None), 503);
    // A phone number id of the wrong shape fails at start.
    let mut options = mock.options();
    options["phone_number_id"] = json!("12345");
    let (kit, _root) = catalog::install("whatsapp_cloud", &options, &[mock.host()]);
    catalog::secret(&kit, "WHATSAPP_CLOUD_ACCESS_TOKEN", TOKEN);
    let err = kit
        .adapter()
        .unwrap()
        .connect(catalog::Link::new().dyn_link())
        .await
        .unwrap_err();
    assert!(err.message.contains("15 to 17 digits"), "{}", err.message);
}

fn choices(n: usize) -> Vec<Button> {
    (1..=n)
        .map(|i| Button {
            label: format!("Option number {i} with a long label"),
            data: format!("clar:q1:0.{i}:0123456789abcdef"),
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn choices_are_buttons_or_lists_and_numbers_answer_them() {
    let mock = Graph::start();
    let (adapter, link, _root) = connected(&mock, &mock.options()).await;
    mock.inject("hello");
    link.wait_for(1).await;
    for n in [3, 7] {
        let mut card = Outgoing::text(USER, None, "Which one?");
        card.buttons = choices(n);
        adapter.send(&card).await.unwrap();
    }
    let sent = mock.payloads();
    let (_, buttons) = &sent[0];
    assert_eq!(buttons["interactive"]["type"], "button");
    let title = buttons["interactive"]["action"]["buttons"][2]["reply"]["title"]
        .as_str()
        .unwrap();
    assert_eq!(title, "Option number 3 with");
    let (_, list) = &sent[1];
    assert_eq!(list["interactive"]["type"], "list");
    let rows = list["interactive"]["action"]["sections"][0]["rows"]
        .as_array()
        .unwrap();
    assert_eq!(rows.len(), 7);
    assert_eq!(rows[6]["title"], "Option number 7 with a l");
    // A numbered answer picks from the latest choice message.
    mock.inject("2");
    let got = link.wait_for(2).await;
    assert_eq!(
        got[1].callback.as_deref(),
        Some("clar:q1:0.2:0123456789abcdef")
    );
    adapter.disconnect().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn approvals_show_approve_and_deny() {
    let mock = Graph::start();
    let (adapter, link, _root) = connected(&mock, &mock.options()).await;
    let mut card = Outgoing::text(USER, None, "Approval needed: terminal");
    card.buttons = ["once", "session", "always", "deny"]
        .iter()
        .map(|c| Button {
            label: (*c).to_owned(),
            data: format!("appr:srq-1:{c}:0123456789abcdef"),
        })
        .collect();
    adapter.send(&card).await.unwrap();
    let shown = mock.sent().pop().unwrap();
    let labels: Vec<&str> = shown.buttons.iter().map(|b| b.label.as_str()).collect();
    assert_eq!(labels, ["Approve", "Deny"]);
    mock.press(&shown.message_id, &shown.buttons[1].data);
    let got = link.wait_for(1).await;
    assert_eq!(
        got[0].callback.as_deref(),
        Some("appr:srq-1:deny:0123456789abcdef")
    );
    adapter.disconnect().await;
}

/// Scenario: Window closed (the adapter's half: the gateway marks the
/// outbox entry failed with the refusal, see
/// `a_cron_delivery_outside_the_window_fails_its_outbox_entry`).
#[tokio::test(flavor = "multi_thread")]
async fn outside_the_window_a_send_fails_or_the_template_invites_back() {
    let mock = Graph::start();
    mock.close_window(USER);
    let (adapter, link, _root) = connected(&mock, &mock.options()).await;
    let err = adapter
        .send(&Outgoing::text(USER, None, "your daily report"))
        .await
        .unwrap_err();
    match err {
        SendError::Permanent(why) => assert_eq!(why, "WhatsApp 24-hour window closed"),
        other => panic!("want a permanent refusal, got {other:?}"),
    }
    adapter.disconnect().await;
    drop(link);

    let mut options = mock.options();
    options["reengagement_template"] = json!("daily_checkin");
    let (adapter, link, _root) = connected(&mock, &options).await;
    adapter
        .send(&Outgoing::text(USER, None, "your daily report"))
        .await
        .unwrap();
    let sent = mock.payloads();
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0].1["template"]["name"], "daily_checkin");
    // The user's answer reopens the window: the held report goes out.
    mock.state.lock().unwrap().closed.clear();
    mock.inject("ok");
    link.wait_for(1).await;
    let texts: Vec<String> = mock.sent().into_iter().map(|m| m.text).collect();
    assert_eq!(texts, ["your daily report"]);
    adapter.disconnect().await;
}

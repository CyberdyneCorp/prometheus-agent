//! The `dingtalk` catalog adapter plugin (spec: platform-adapters, `DingTalk`
//! adapter; Catalog adapter set) against a mock `DingTalk` serving the
//! Stream Mode gateway (WebSocket, held by the bridge sidecar) and the
//! `OpenAPI` on one port: the conformance suite (replies as AI Cards, edits
//! streaming into them), session-webhook replies and their expiry, the
//! Thinking/Done emotions and group mention gating.

#![cfg(feature = "gateway")]

mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use common::catalog::{self, Bridge, Reply, Request};
use common::catalog_ws::{Socket, WsServer};
use pmt_gateway::messaging::Outgoing;
use pmt_gateway::messaging::conformance::{Failure, MockPlatform, MockSent};
use serde_json::{Value, json};

const SECRET: &str = "dt-secret";
const DM: &str = "cid-dm";
const GROUP: &str = "cid-group";
const USER: &str = "staff1";

#[derive(Default)]
struct State {
    queued: Vec<String>,
    last: Option<String>,
    /// Session-webhook messages and cards: (chat, id, text).
    sent: Vec<MockSent>,
    /// Card ids created.
    cards: Vec<String>,
    /// Emotions: (message, name, recalled).
    emotions: Vec<(String, String, bool)>,
    acks: Vec<String>,
    fail_next: Option<Failure>,
}

struct DingTalk {
    server: WsServer,
    state: Arc<Mutex<State>>,
    next: AtomicU64,
}

fn fail(state: &mut State) -> Option<Reply> {
    Some(match state.fail_next.take()? {
        Failure::RateLimited(_) => Reply::json(429, &json!({"code": "Throttling"})),
        Failure::ServerError => Reply::status(500),
        Failure::Forbidden | Failure::Markup => Reply::json(403, &json!({"code": "Forbidden"})),
    })
}

fn rest(port: u16, state: &Mutex<State>, request: &Request) -> Reply {
    let mut state = state.lock().unwrap();
    let body = request.json();
    let route = request.route().to_owned();
    let authorized = request.header("x-acs-dingtalk-access-token") == Some("at");
    match (request.method.as_str(), route.as_str()) {
        ("POST", "/v1.0/gateway/connections/open") if body["clientSecret"] == SECRET => {
            Reply::json(
                200,
                &json!({"endpoint": format!("ws://127.0.0.1:{port}/stream"), "ticket": "tk"}),
            )
        }
        ("POST", "/v1.0/oauth2/accessToken") if body["appSecret"] == SECRET => {
            Reply::json(200, &json!({"accessToken": "at", "expireIn": 7200}))
        }
        ("POST", path) if path.starts_with("/v1.0/robot/emotion/") && authorized => {
            state.emotions.push((
                body["openMsgId"].as_str().unwrap_or_default().to_owned(),
                body["emotionName"].as_str().unwrap_or_default().to_owned(),
                path.ends_with("recall"),
            ));
            Reply::json(200, &json!({}))
        }
        ("POST", "/v1.0/card/instances/createAndDeliver") if authorized => {
            if let Some(reply) = fail(&mut state) {
                return reply;
            }
            let track = body["outTrackId"].as_str().unwrap_or_default().to_owned();
            let chat = body["openSpaceId"]
                .as_str()
                .unwrap_or_default()
                .trim_start_matches("dtv1.card//IM_ROBOT.")
                .to_owned();
            state.cards.push(track.clone());
            state.sent.push(MockSent {
                chat_id: chat,
                message_id: track,
                text: String::new(),
                buttons: Vec::new(),
                thread_id: None,
            });
            Reply::json(200, &json!({"result": {}}))
        }
        ("PUT", "/v1.0/card/streaming") if authorized => {
            let track = body["outTrackId"].as_str().unwrap_or_default();
            if let Some(card) = state.sent.iter_mut().find(|s| s.message_id == track) {
                body["content"]
                    .as_str()
                    .unwrap_or_default()
                    .clone_into(&mut card.text);
            }
            Reply::json(200, &json!({}))
        }
        ("POST", "/robot/sendBySession") => {
            if let Some(reply) = fail(&mut state) {
                return reply;
            }
            let chat = request
                .path
                .split("session=")
                .nth(1)
                .unwrap_or_default()
                .to_owned();
            if chat == "expired" {
                return Reply::json(
                    200,
                    &json!({"errcode": 300_001, "errmsg": "session expired"}),
                );
            }
            let id = format!("s{}", state.sent.len() + 1);
            let text = body["markdown"]["text"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            state.sent.push(MockSent {
                chat_id: chat,
                message_id: id,
                text,
                buttons: Vec::new(),
                thread_id: None,
            });
            Reply::json(200, &json!({"errcode": 0}))
        }
        _ => Reply::json(401, &json!({"code": "InvalidAuthentication"})),
    }
}

fn callback(
    port: u16,
    id: &str,
    chat: &str,
    group: bool,
    text: &str,
    at: bool,
    lifetime_ms: u64,
) -> String {
    let data = json!({
        "conversationId": chat, "conversationType": if group { "2" } else { "1" },
        "msgId": id, "senderStaffId": USER, "senderNick": "Kit", "isInAtList": at,
        "text": {"content": text},
        "sessionWebhook": format!("http://127.0.0.1:{port}/robot/sendBySession?session={chat}"),
        "createAt": 1_000_000u64, "sessionWebhookExpiredTime": 1_000_000 + lifetime_ms,
    });
    json!({"specVersion": "1.0", "type": "CALLBACK",
        "headers": {"topic": "/v1.0/im/bot/messages/get", "messageId": format!("frame-{id}")},
        "data": data.to_string()})
    .to_string()
}

impl DingTalk {
    async fn start() -> Arc<Self> {
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
                    socket.send(frame);
                }
                true
            }),
            Arc::new(move |_: &Socket, frame: &str| {
                on_frame.lock().unwrap().acks.push(frame.to_owned());
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

    fn deliver(&self, frame: String) {
        self.state.lock().unwrap().last = Some(frame.clone());
        if !self.server.broadcast(&frame) {
            self.state.lock().unwrap().queued.push(frame);
        }
    }

    fn message(&self, chat: &str, group: bool, text: &str, at: bool, lifetime_ms: u64) -> String {
        let id = format!("msg{}", self.next.fetch_add(1, Ordering::Relaxed));
        self.deliver(callback(
            self.server.port,
            &id,
            chat,
            group,
            text,
            at,
            lifetime_ms,
        ));
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
            "client_id": "ding-app",
            "api_base": self.server.base(),
            "session_webhook_base": format!("{}/robot/", self.server.base()),
            "sidecar": bridge.sidecar,
        });
        for (k, v) in extra.as_object().into_iter().flatten() {
            options[k] = v.clone();
        }
        let hosts = [
            self.server.host(),
            bridge.base.trim_start_matches("http://").to_owned(),
        ];
        let (kit, root) = catalog::install("dingtalk", &options, &hosts);
        catalog::secret(&kit, "DINGTALK_CLIENT_SECRET", SECRET);
        (kit, root)
    }
}

impl MockPlatform for DingTalk {
    fn dm(&self) -> (String, String) {
        (DM.to_owned(), USER.to_owned())
    }

    fn inject(&self, text: &str) -> String {
        self.message(DM, false, text, false, 3_600_000)
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
}

#[tokio::test(flavor = "multi_thread")]
async fn the_dingtalk_adapter_passes_the_conformance_suite() {
    let dt = DingTalk::start().await;
    let bridge = Bridge::start(&[dt.server.host()]);
    let (kit, _root) = dt.install(&bridge, &json!({"card_template_id": "tpl.schema"}));
    let report = kit
        .run(
            kit.adapter().unwrap(),
            Arc::clone(&dt) as Arc<dyn MockPlatform>,
        )
        .await;
    catalog::assert_passed(&report);
}

/// Scenario: AI card streaming.
#[tokio::test(flavor = "multi_thread")]
async fn a_streamed_reply_updates_one_ai_card() {
    let dt = DingTalk::start().await;
    let bridge = Bridge::start(&[dt.server.host()]);
    let (kit, _root) = dt.install(&bridge, &json!({"card_template_id": "tpl.schema"}));
    let adapter = kit.adapter().unwrap();
    let link = catalog::Link::new();
    adapter.connect(link.dyn_link()).await.unwrap();
    let asked = dt.message(DM, false, "tell me a story", false, 3_600_000);
    assert_eq!(link.wait_for(1).await.len(), 1);
    let sent = adapter
        .send(&Outgoing::text(DM, None, "Once"))
        .await
        .unwrap();
    let id = sent.message_id.unwrap();
    for text in ["Once upon", "Once upon a time."] {
        adapter
            .edit(&id, &Outgoing::text(DM, None, text))
            .await
            .unwrap();
    }
    let state = std::mem::take(&mut *dt.state.lock().unwrap());
    assert_eq!(
        state.cards,
        std::slice::from_ref(&id),
        "one card, no other messages"
    );
    assert_eq!(state.sent.len(), 1);
    assert_eq!(state.sent[0].text, "Once upon a time.");
    // The callback was acknowledged, and Thinking became Done.
    assert!(
        state
            .acks
            .iter()
            .any(|a| a.contains(&format!("frame-{asked}")))
    );
    let emotions: Vec<(&str, bool)> = state
        .emotions
        .iter()
        .map(|(_, n, r)| (n.as_str(), *r))
        .collect();
    assert_eq!(
        emotions,
        [
            ("🤔Thinking", false),
            ("🤔Thinking", true),
            ("🥳Done", false)
        ]
    );
    adapter.disconnect().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn replies_use_the_session_webhook_until_it_expires_and_groups_need_a_mention() {
    let dt = DingTalk::start().await;
    let bridge = Bridge::start(&[dt.server.host()]);
    let (kit, _root) = dt.install(&bridge, &json!({"mention_patterns": ["小助手"]}));
    let adapter = kit.adapter().unwrap();
    let link = catalog::Link::new();
    adapter.connect(link.dyn_link()).await.unwrap();
    dt.message(GROUP, true, "chatter", false, 3_600_000);
    dt.message(GROUP, true, "@bot hi", true, 3_600_000);
    dt.message(GROUP, true, "小助手 在吗", false, 3_600_000);
    // A webhook that is already expired cannot carry the reply.
    dt.message(DM, false, "late", false, 0);
    let admitted = link.wait_for(3).await;
    let texts: Vec<&str> = admitted.iter().map(|i| i.text.as_str()).collect();
    assert_eq!(texts, ["@bot hi", "小助手 在吗", "late"]);
    adapter
        .send(&Outgoing::text(GROUP, None, "**hello**"))
        .await
        .unwrap();
    assert_eq!(dt.sent()[0].chat_id, GROUP);
    assert_eq!(dt.sent()[0].text, "**hello**");
    let err = adapter
        .send(&Outgoing::text(DM, None, "too late"))
        .await
        .unwrap_err();
    assert!(
        err.describe().contains("No session_webhook available"),
        "{err:?}"
    );
    adapter.disconnect().await;
}

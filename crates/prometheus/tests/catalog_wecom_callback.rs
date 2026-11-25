//! The `wecom_callback` catalog adapter plugin (spec: platform-adapters,
//! WeCom callback adapter; Catalog adapter set) against a mock WeCom: the
//! conformance suite, `WXBizMsgCrypt` callbacks (URL verification, a bad
//! signature refused), and two apps in two corps each answered with its
//! own access token.

#![cfg(feature = "gateway")]

mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use common::catalog::{self, MockHttp, Reply, Request};
use common::catalog_crypt::{encoding_aes_key, seal, signature};
use pmt_gateway::messaging::Outgoing;
use pmt_gateway::messaging::conformance::{Failure, MockPlatform, MockSent};
use serde_json::{Value, json};

/// One app: corp, agent, verification token, key, corp secret.
struct App {
    corp: &'static str,
    agent: i64,
    token: &'static str,
    aes: (String, [u8; 32]),
    secret: &'static str,
}

fn apps() -> [App; 2] {
    [
        App {
            corp: "wwcorpA",
            agent: 1_000_001,
            token: "tokenA",
            aes: encoding_aes_key(3),
            secret: "secretA",
        },
        App {
            corp: "wwcorpB",
            agent: 1_000_002,
            token: "tokenB",
            aes: encoding_aes_key(9),
            secret: "secretB",
        },
    ]
}

#[derive(Default)]
struct State {
    adapter: Option<String>,
    /// Sends: (access token, touser, agent id, text).
    sends: Vec<(String, String, i64, String)>,
    sent: Vec<MockSent>,
    fail_next: Option<Failure>,
    last: Option<(usize, String, String)>,
}

struct WeCom {
    api: MockHttp,
    apps: [App; 2],
    state: Arc<Mutex<State>>,
    next: AtomicU64,
}

fn answer(state: &Mutex<State>, request: &Request) -> Reply {
    let mut state = state.lock().unwrap();
    let query: Vec<(String, String)> = url::form_urlencoded::parse(
        request
            .path
            .split_once('?')
            .map_or("", |(_, q)| q)
            .as_bytes(),
    )
    .into_owned()
    .collect();
    let get = |k: &str| {
        query
            .iter()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    match request.route() {
        "/cgi-bin/gettoken" => {
            let token = match (get("corpid").as_str(), get("corpsecret").as_str()) {
                ("wwcorpA", "secretA") => "AT-A",
                ("wwcorpB", "secretB") => "AT-B",
                _ => {
                    return Reply::json(
                        200,
                        &json!({"errcode": 40013, "errmsg": "invalid corpid"}),
                    );
                }
            };
            Reply::json(
                200,
                &json!({"errcode": 0, "access_token": token, "expires_in": 7200}),
            )
        }
        "/cgi-bin/message/send" => {
            match state.fail_next.take() {
                Some(Failure::RateLimited(_)) => {
                    return Reply::json(429, &json!({"errcode": 45009}));
                }
                Some(Failure::ServerError) => return Reply::status(500),
                Some(Failure::Forbidden | Failure::Markup) => {
                    return Reply::json(200, &json!({"errcode": 60020, "errmsg": "not allow"}));
                }
                None => {}
            }
            let body = request.json();
            let text = body["text"]["content"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            if text.len() > 2048 {
                return Reply::json(200, &json!({"errcode": 44004, "errmsg": "too long"}));
            }
            let token = get("access_token");
            let user = body["touser"].as_str().unwrap_or_default().to_owned();
            let corp = if token == "AT-A" {
                "wwcorpA"
            } else {
                "wwcorpB"
            };
            let id = format!("msg{}", state.sent.len() + 1);
            state.sends.push((
                token,
                user.clone(),
                body["agentid"].as_i64().unwrap_or(0),
                text.clone(),
            ));
            state.sent.push(MockSent {
                chat_id: format!("{corp}:{user}"),
                message_id: id.clone(),
                text,
                buttons: Vec::new(),
                thread_id: None,
            });
            Reply::json(200, &json!({"errcode": 0, "errmsg": "ok", "msgid": id}))
        }
        _ => Reply::status(404),
    }
}

impl WeCom {
    fn start() -> Arc<Self> {
        let state = Arc::new(Mutex::new(State::default()));
        let served = Arc::clone(&state);
        let api = MockHttp::start(Arc::new(move |request: &Request| answer(&served, request)));
        Arc::new(Self {
            api,
            apps: apps(),
            state,
            next: AtomicU64::new(1),
        })
    }

    fn options(&self) -> Value {
        json!({
            "api_base": self.api.base(),
            "apps": self.apps.iter().map(|a| json!({"name": a.corp, "corp_id": a.corp, "agent_id": a.agent})).collect::<Vec<_>>(),
        })
    }

    fn secrets(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for (i, app) in self.apps.iter().enumerate() {
            let n = i + 1;
            out.push((
                format!("WECOM_CALLBACK_CORP_SECRET_{n}"),
                app.secret.to_owned(),
            ));
            out.push((format!("WECOM_CALLBACK_TOKEN_{n}"), app.token.to_owned()));
            out.push((format!("WECOM_CALLBACK_AES_KEY_{n}"), app.aes.0.clone()));
        }
        out
    }

    fn base(&self) -> String {
        self.state
            .lock()
            .unwrap()
            .adapter
            .clone()
            .expect("the adapter listens")
    }

    /// Posts a text callback from `user` through app `index`, signed with
    /// that app's token unless `forged`: the status.
    fn deliver(&self, index: usize, id: &str, user: &str, text: &str, forged: bool) -> u16 {
        let app = &self.apps[index];
        let xml = format!(
            "<xml><ToUserName><![CDATA[{corp}]]></ToUserName><FromUserName><![CDATA[{user}]]></FromUserName><CreateTime>1700000000</CreateTime><MsgType><![CDATA[text]]></MsgType><Content><![CDATA[{text}]]></Content><MsgId>{id}</MsgId><AgentID>{agent}</AgentID></xml>",
            corp = app.corp,
            agent = app.agent
        );
        let encrypt = seal(&app.aes.1, &xml, app.corp);
        let token = if forged { "not-the-token" } else { app.token };
        let sig = signature(token, "1700000000", "nonce1", &encrypt);
        let body = format!(
            "<xml><ToUserName><![CDATA[{}]]></ToUserName><Encrypt><![CDATA[{encrypt}]]></Encrypt></xml>",
            app.corp
        );
        let url = format!(
            "{}/?msg_signature={sig}&timestamp=1700000000&nonce=nonce1",
            self.base()
        );
        catalog::post(&url, &[("content-type", "text/xml")], body.as_bytes()).0
    }
}

impl MockPlatform for WeCom {
    fn dm(&self) -> (String, String) {
        ("wwcorpA:alice".to_owned(), "wwcorpA:alice".to_owned())
    }

    fn inject(&self, text: &str) -> String {
        let id = format!("{}", 7_000 + self.next.fetch_add(1, Ordering::Relaxed));
        self.state.lock().unwrap().last = Some((0, id.clone(), text.to_owned()));
        assert_eq!(self.deliver(0, &id, "alice", text, false), 200);
        id
    }

    fn redeliver(&self) {
        let last = self.state.lock().unwrap().last.clone();
        if let Some((index, id, text)) = last {
            self.deliver(index, &id, "alice", &text, false);
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

    fn sent(&self) -> Vec<MockSent> {
        self.state.lock().unwrap().sent.clone()
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

    fn hints_retry_after(&self) -> bool {
        false
    }
}

fn install(
    wecom: &WeCom,
) -> (
    prometheus::plugins_cmd::adapter_plugin::PluginKit,
    tempfile::TempDir,
) {
    let (kit, root) = catalog::install("wecom_callback", &wecom.options(), &[wecom.api.host()]);
    for (name, value) in wecom.secrets() {
        catalog::secret(&kit, &name, &value);
    }
    (kit, root)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_wecom_callback_adapter_passes_the_conformance_suite() {
    let wecom = WeCom::start();
    let (kit, _root) = install(&wecom);
    let report = kit
        .run(
            kit.adapter().unwrap(),
            Arc::clone(&wecom) as Arc<dyn MockPlatform>,
        )
        .await;
    catalog::assert_passed(&report);
}

/// Scenario: Multi-app routing.
#[tokio::test(flavor = "multi_thread")]
async fn a_user_of_the_second_app_is_answered_with_its_token() {
    let wecom = WeCom::start();
    let (kit, _root) = install(&wecom);
    let adapter = kit.adapter().unwrap();
    let link = catalog::Link::new();
    let prefix = catalog::serve(Arc::clone(&adapter)).await;
    wecom.listen(&prefix);
    adapter.connect(link.dyn_link()).await.unwrap();
    assert_eq!(wecom.deliver(0, "1", "alice", "hello from A", false), 200);
    assert_eq!(wecom.deliver(1, "2", "bob", "hello from B", false), 200);
    let admitted = link.wait_for(2).await;
    let chats: Vec<&str> = admitted.iter().map(|i| i.chat_id.as_str()).collect();
    assert_eq!(chats, ["wwcorpA:alice", "wwcorpB:bob"]);
    adapter
        .send(&Outgoing::text("wwcorpB:bob", None, "hi bob"))
        .await
        .unwrap();
    let sends = wecom.state.lock().unwrap().sends.clone();
    assert_eq!(
        sends,
        [(
            "AT-B".to_owned(),
            "bob".to_owned(),
            1_000_002,
            "hi bob".to_owned()
        )]
    );
    adapter.disconnect().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn url_verification_answers_the_echo_and_forged_callbacks_are_refused() {
    let wecom = WeCom::start();
    let (kit, _root) = install(&wecom);
    let adapter = kit.adapter().unwrap();
    let link = catalog::Link::new();
    let prefix = catalog::serve(Arc::clone(&adapter)).await;
    wecom.listen(&prefix);
    adapter.connect(link.dyn_link()).await.unwrap();
    let app = &wecom.apps[1];
    let echo = seal(&app.aes.1, "echo-4711", app.corp);
    let sig = signature(app.token, "1", "n", &echo);
    let query: String = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs([
            ("msg_signature", sig.as_str()),
            ("timestamp", "1"),
            ("nonce", "n"),
            ("echostr", echo.as_str()),
        ])
        .finish();
    let (status, body) = catalog::request("GET", &format!("{prefix}/?{query}"), &[], b"");
    assert_eq!(
        (status, String::from_utf8_lossy(&body).into_owned()),
        (200, "echo-4711".to_owned())
    );
    assert_eq!(wecom.deliver(0, "9", "mallory", "let me in", true), 401);
    assert!(link.wait_for(1).await.is_empty());
    adapter.disconnect().await;
}

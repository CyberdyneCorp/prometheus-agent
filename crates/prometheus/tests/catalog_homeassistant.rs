//! The `homeassistant` catalog adapter plugin and toolset (spec:
//! platform-adapters, Home Assistant adapter and toolset; Catalog adapter
//! set) against a mock Home Assistant serving its WebSocket and REST APIs
//! on one port, the WebSocket held by the socket bridge (system Node).

#![cfg(feature = "gateway")]

mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use common::catalog::{self, Bridge};
use futures_util::{SinkExt as _, StreamExt as _};
use pmt_config::schema::PlatformConfig;
use pmt_core::cancel::CancelToken;
use pmt_gateway::messaging::conformance::{Failure, MockPlatform, MockSent};
use pmt_tools::descriptor::{CallContext, Executor};
use pmt_tools::platform_tools::{Context, Contributor, SecretReader};
use prometheus::platform_tools_host::homeassistant;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

const TOKEN: &str = "hass-token";

#[derive(Default)]
struct State {
    /// The live WebSocket's outbox (once subscribed).
    socket: Option<mpsc::UnboundedSender<String>>,
    /// Events for the adapter while it is away.
    queued: Vec<String>,
    last: Option<String>,
    sent: Vec<MockSent>,
    fail_next: Option<Failure>,
    pings: usize,
}

/// A mock Home Assistant.
struct Hass {
    port: u16,
    state: Arc<Mutex<State>>,
    next: AtomicU64,
}

fn event(n: u64, text: &str) -> String {
    json!({
        "id": 1,
        "type": "event",
        "event": {
            "event_type": "state_changed",
            "context": {"id": format!("ctx{n}")},
            "data": {
                "entity_id": format!("sensor.kit_{n}"),
                "old_state": {"state": "idle"},
                "new_state": {"state": text, "attributes": {"friendly_name": "Kit"}},
            },
        },
    })
    .to_string()
}

async fn websocket(state: Arc<Mutex<State>>, stream: TcpStream) {
    let Ok(mut socket) = tokio_tungstenite::accept_async(stream).await else {
        return;
    };
    // The sender lives in `State` once subscribed: dropping it there (a
    // dropped connection) ends this task and closes the socket.
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let mut tx = Some(tx);
    let _ = socket
        .send(Message::text(json!({"type": "auth_required"}).to_string()))
        .await;
    loop {
        tokio::select! {
            out = rx.recv() => match out {
                Some(frame) => { let _ = socket.send(Message::text(frame)).await; }
                None => break,
            },
            incoming = socket.next() => {
                let Some(Ok(Message::Text(text))) = incoming else { break };
                let message: Value = serde_json::from_str(&text).unwrap_or_default();
                match message["type"].as_str() {
                    Some("auth") if message["access_token"] == TOKEN => {
                        let _ = socket.send(Message::text(json!({"type": "auth_ok"}).to_string())).await;
                    }
                    Some("auth") => {
                        let _ = socket.send(Message::text(json!({"type": "auth_invalid"}).to_string())).await;
                    }
                    Some("subscribe_events") => {
                        let _ = socket
                            .send(Message::text(json!({"id": message["id"], "type": "result", "success": true}).to_string()))
                            .await;
                        let mut state = state.lock().unwrap();
                        if let Some(tx) = tx.take() {
                            for frame in state.queued.drain(..) {
                                let _ = tx.send(frame);
                            }
                            state.socket = Some(tx);
                        }
                    }
                    Some("ping") => state.lock().unwrap().pings += 1,
                    _ => {}
                }
            }
        }
    }
}

async fn rest(state: Arc<Mutex<State>>, mut stream: TcpStream) {
    let mut raw = Vec::new();
    let mut buf = [0u8; 8192];
    let (head, body) = loop {
        let Ok(n) = stream.read(&mut buf).await else {
            return;
        };
        if n == 0 {
            return;
        }
        raw.extend_from_slice(&buf[..n]);
        let text = String::from_utf8_lossy(&raw).into_owned();
        if let Some((head, body)) = text.split_once("\r\n\r\n") {
            let length: usize = head
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().parse().unwrap_or(0))
                })
                .unwrap_or(0);
            if body.len() >= length {
                break (head.to_owned(), body.to_owned());
            }
        }
    };
    let path = head
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_owned();
    let authorized = head
        .to_ascii_lowercase()
        .contains(&format!("authorization: bearer {TOKEN}"));
    let (status, answer) = {
        let mut state = state.lock().unwrap();
        match (authorized, state.fail_next.take()) {
            (false, _) => (401, "{}".to_owned()),
            (_, Some(Failure::RateLimited(_))) => (429, "{}".to_owned()),
            (_, Some(Failure::ServerError)) => (500, "{}".to_owned()),
            (_, Some(Failure::Forbidden | Failure::Markup)) => (403, "{}".to_owned()),
            (true, None) => {
                let body: Value = serde_json::from_str(&body).unwrap_or_default();
                let chat = match path.strip_prefix("/api/services/notify/") {
                    Some(target) => target.to_owned(),
                    None => "events".to_owned(),
                };
                let id = state.sent.len().to_string();
                state.sent.push(MockSent {
                    chat_id: chat,
                    message_id: id,
                    text: body["message"].as_str().unwrap_or_default().to_owned(),
                    buttons: Vec::new(),
                    thread_id: None,
                });
                (200, "[]".to_owned())
            }
        }
    };
    let reply = format!(
        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{answer}",
        answer.len()
    );
    let _ = stream.write_all(reply.as_bytes()).await;
}

impl Hass {
    async fn start() -> Arc<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(Mutex::new(State::default()));
        let served = Arc::clone(&state);
        pmt_core::spawn::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let state = Arc::clone(&served);
                pmt_core::spawn::spawn(async move {
                    let mut peek = [0u8; 2048];
                    let n = stream.peek(&mut peek).await.unwrap_or(0);
                    let head = String::from_utf8_lossy(&peek[..n]).to_ascii_lowercase();
                    if head.contains("upgrade: websocket") {
                        websocket(state, stream).await;
                    } else {
                        rest(state, stream).await;
                    }
                });
            }
        });
        Arc::new(Self {
            port,
            state,
            next: AtomicU64::new(1),
        })
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn options(&self, bridge: &Bridge) -> Value {
        json!({
            "url": self.url(),
            "watch_domains": ["sensor"],
            "event_template": "{new}",
            "sidecar": bridge.sidecar,
        })
    }

    fn deliver(&self, frame: String) {
        let mut state = self.state.lock().unwrap();
        state.last = Some(frame.clone());
        let live = state
            .socket
            .as_ref()
            .is_some_and(|s| s.send(frame.clone()).is_ok());
        if !live {
            state.socket = None;
            state.queued.push(frame);
        }
    }
}

impl MockPlatform for Hass {
    fn dm(&self) -> (String, String) {
        ("events".to_owned(), "homeassistant".to_owned())
    }

    fn inject(&self, text: &str) -> String {
        let n = self.next.fetch_add(1, Ordering::Relaxed);
        self.deliver(event(n, text));
        format!("ctx{n}")
    }

    fn redeliver(&self) {
        let last = self.state.lock().unwrap().last.clone();
        if let Some(frame) = last {
            self.deliver(frame);
        }
    }

    fn drop_connection(&self) {
        // Dropping the outbox ends the socket task, which closes it.
        self.state.lock().unwrap().socket = None;
    }

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

    fn hints_retry_after(&self) -> bool {
        false
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_home_assistant_adapter_passes_the_conformance_suite() {
    let hass = Hass::start().await;
    let bridge = Bridge::start(&[format!("127.0.0.1:{}", hass.port)]);
    let report = catalog::conformance(
        "homeassistant",
        &hass.options(&bridge),
        &[
            bridge.base.trim_start_matches("http://").to_owned(),
            format!("127.0.0.1:{}", hass.port),
        ],
        &[("HASS_TOKEN", TOKEN)],
        Arc::clone(&hass) as Arc<dyn MockPlatform>,
    )
    .await;
    catalog::assert_passed(&report);
}

#[tokio::test(flavor = "multi_thread")]
async fn only_watched_entities_arrive_once_per_cooldown_and_cron_targets_notify() {
    let hass = Hass::start().await;
    let bridge = Bridge::start(&[format!("127.0.0.1:{}", hass.port)]);
    let mut options = hass.options(&bridge);
    options["watch_domains"] = json!(["light"]);
    options["ignore_entities"] = json!(["light.hall"]);
    options["event_template"] = json!("{name} ({entity_id}) changed from {old} to {new}");
    let (kit, _root) = catalog::install(
        "homeassistant",
        &options,
        &[
            bridge.base.trim_start_matches("http://").to_owned(),
            format!("127.0.0.1:{}", hass.port),
        ],
    );
    catalog::secret(&kit, "HASS_TOKEN", TOKEN);
    let adapter = kit.adapter().unwrap();
    let link = catalog::Link::new();
    adapter.connect(link.dyn_link()).await.unwrap();
    let change = |id: &str, entity: &str, new: &str| {
        json!({"type": "event", "event": {"event_type": "state_changed", "context": {"id": id},
            "data": {"entity_id": entity, "old_state": {"state": "off"},
                     "new_state": {"state": new, "attributes": {"friendly_name": "Kitchen"}}}}})
        .to_string()
    };
    hass.deliver(change("a", "light.kitchen", "on"));
    hass.deliver(change("b", "light.hall", "on")); // ignored
    hass.deliver(change("c", "switch.fan", "on")); // not watched
    hass.deliver(change("d", "light.kitchen", "off")); // within the cooldown
    let admitted = link.wait_for(1).await;
    assert_eq!(admitted.len(), 1, "{admitted:?}");
    assert_eq!(
        admitted[0].text,
        "Kitchen (light.kitchen) changed from off to on"
    );
    assert_eq!(admitted[0].sender_id, "homeassistant");
    // `homeassistant:mobile_app_phone` cron deliveries use the notify service.
    let out = pmt_gateway::messaging::Outgoing::text("mobile_app_phone", None, "Door left open");
    adapter.send(&out).await.unwrap();
    let sent = hass.sent();
    assert_eq!(sent[0].chat_id, "mobile_app_phone");
    assert_eq!(sent[0].text, "Door left open");
    adapter.disconnect().await;
}

fn tools(url: &str) -> Vec<pmt_tools::RegisteredTool> {
    let config: PlatformConfig =
        serde_json::from_value(json!({"enabled": true, "options": {"url": url}})).unwrap();
    let dir = catalog::scratch();
    let secrets: SecretReader = Arc::new(|name| (name == "HASS_TOKEN").then(|| TOKEN.to_owned()));
    let context = Context {
        profile_dir: dir.path(),
        config: &config,
        granted: None,
        secrets,
        network: &pmt_net::http::OutboundPolicy::default(),
    };
    homeassistant::Tools.tools(&context)
}

async fn call(
    tools: &[pmt_tools::RegisteredTool],
    name: &str,
    args: Value,
) -> Result<String, String> {
    let tool = tools.iter().find(|t| t.name() == name).unwrap();
    let Executor::Handler(handler) = &tool.executor else {
        panic!("not a handler")
    };
    let context = CallContext {
        call_id: "c".into(),
        session_id: "s".into(),
        cancel: CancelToken::new(),
    };
    handler
        .call(&context, args)
        .await
        .map(|r| serde_json::to_string(&r.content).unwrap())
        .map_err(|f| f.message)
}

/// Scenario: Dangerous domain blocked.
#[tokio::test(flavor = "multi_thread")]
async fn a_service_of_a_blocked_domain_is_refused_naming_it() {
    let hass = Hass::start().await;
    let all = tools(&hass.url());
    let names: Vec<&str> = all.iter().map(pmt_tools::RegisteredTool::name).collect();
    assert_eq!(
        names,
        [
            "ha_list_entities",
            "ha_get_state",
            "ha_list_services",
            "ha_call_service"
        ]
    );
    let err = call(
        &all,
        "ha_call_service",
        json!({"domain": "shell_command", "service": "run"}),
    )
    .await
    .unwrap_err();
    assert!(err.contains("shell_command"), "{err}");
    assert!(hass.sent().is_empty(), "nothing reached Home Assistant");
    let err = call(&all, "ha_get_state", json!({"entity_id": "../config"}))
        .await
        .unwrap_err();
    assert!(err.contains("entity_id"), "{err}");
    call(
        &all,
        "ha_call_service",
        json!({"domain": "notify", "service": "phone", "data": {"message": "hi"}}),
    )
    .await
    .unwrap();
    assert_eq!(hass.sent()[0].text, "hi");
}

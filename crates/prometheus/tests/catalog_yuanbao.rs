//! The `yuanbao` catalog adapter plugin (spec: platform-adapters, Yuanbao
//! adapter; Feishu and Yuanbao tools) against a mock Yuanbao gateway:
//! the HMAC-signed token, the protobuf WebSocket (held by the bridge
//! sidecar in binary mode), the object store and the media CDN on one
//! port. The conformance suite, forwarded chat history and the `yuanbao`
//! toolset acting over the adapter's connection.

#![cfg(feature = "gateway")]

mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use common::catalog::{self, Bridge, Reply, Request};
use common::catalog_ws::{Socket, WsServer};
use pmt_gateway::messaging::MediaKind;
use pmt_gateway::messaging::conformance::{
    Failure, MockFile, MockPlatform, MockSent, kind_of_name,
};
use pmt_tools::platform_tools::{Context, Contributor as _, SecretReader};
use serde_json::{Value, json};

const APP_KEY: &str = "yb-app";
const SECRET: &str = "yb-secret";
const TOKEN: &str = "yb-token";
const BOT: &str = "bot-1";
const USER: &str = "alice";

/// A protobuf message, field by field.
#[derive(Default, Clone)]
struct Pb(Vec<u8>);

fn varint(mut value: u64, out: &mut Vec<u8>) {
    loop {
        let bits = u8::try_from(value & 0x7f).unwrap();
        value >>= 7;
        if value == 0 {
            out.push(bits);
            return;
        }
        out.push(bits | 0x80);
    }
}

impl Pb {
    fn int(mut self, n: u32, value: u64) -> Self {
        varint(u64::from(n) << 3, &mut self.0);
        varint(value, &mut self.0);
        self
    }

    fn bytes(mut self, n: u32, value: &[u8]) -> Self {
        varint((u64::from(n) << 3) | 2, &mut self.0);
        varint(value.len() as u64, &mut self.0);
        self.0.extend_from_slice(value);
        self
    }

    fn string(self, n: u32, value: &str) -> Self {
        self.bytes(n, value.as_bytes())
    }

    fn message(self, n: u32, value: &Self) -> Self {
        self.bytes(n, &value.0)
    }
}

/// A parsed message: (field, varint or bytes).
#[derive(Default, Debug, Clone)]
struct Fields(Vec<(u32, Result<u64, Vec<u8>>)>);

fn read_varint(data: &[u8], at: &mut usize) -> u64 {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = data[*at];
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            break;
        }
    }
    value
}

impl Fields {
    fn parse(data: &[u8]) -> Self {
        let (mut fields, mut at) = (Vec::new(), 0);
        while at < data.len() {
            let tag = read_varint(data, &mut at);
            let n = u32::try_from(tag >> 3).unwrap();
            if tag.trailing_zeros() >= 3 {
                fields.push((n, Ok(read_varint(data, &mut at))));
            } else {
                let len = usize::try_from(read_varint(data, &mut at)).unwrap();
                fields.push((n, Err(data[at..at + len].to_vec())));
                at += len;
            }
        }
        Self(fields)
    }

    fn all(&self, n: u32) -> Vec<Vec<u8>> {
        self.0
            .iter()
            .filter(|(f, _)| *f == n)
            .filter_map(|(_, v)| v.clone().err())
            .collect()
    }

    fn string(&self, n: u32) -> String {
        self.all(n)
            .first()
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .unwrap_or_default()
    }

    fn int(&self, n: u32) -> u64 {
        self.0
            .iter()
            .find_map(|(f, v)| (*f == n).then(|| v.clone().ok()).flatten())
            .unwrap_or(0)
    }

    fn message(&self, n: u32) -> Self {
        Self::parse(&self.all(n).first().cloned().unwrap_or_default())
    }
}

/// A gateway frame: `ConnMsg {head = 1, data = 2}`.
fn frame(cmd_type: u64, cmd: &str, msg_id: &str, status: u64, data: &Pb) -> Vec<u8> {
    let mut head = Pb::default()
        .int(1, cmd_type)
        .string(2, cmd)
        .string(4, msg_id)
        .string(5, "yuanbao_openclaw_proxy");
    if status != 0 {
        head = head.int(10, status);
    }
    Pb::default().message(1, &head).message(2, data).0
}

fn element(kind: &str, content: &Pb) -> Pb {
    Pb::default().string(1, kind).message(2, content)
}

fn text_element(text: &str) -> Pb {
    element("TIMTextElem", &Pb::default().string(1, text))
}

#[derive(Default)]
struct State {
    queued: Vec<Vec<u8>>,
    last: Option<Vec<u8>>,
    bound: bool,
    sent: Vec<MockSent>,
    /// Running-status heartbeats (state 1).
    beats: usize,
    media: Vec<(String, Vec<u8>)>,
    store: Vec<(String, Vec<u8>)>,
    files: Vec<MockFile>,
    /// Group code -> members (user id, nickname).
    members: Vec<(String, String)>,
    fail_next: Option<Failure>,
}

struct Yuanbao {
    server: WsServer,
    state: Arc<Mutex<State>>,
    next: AtomicU64,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut out, b| {
        use std::fmt::Write as _;
        let _ = write!(out, "{b:02x}");
        out
    })
}

fn sign_token(request: &Request) -> Reply {
    let body = request.json();
    let field = |k: &str| body[k].as_str().unwrap_or_default().to_owned();
    let plain = format!(
        "{}{}{}{SECRET}",
        field("nonce"),
        field("timestamp"),
        field("app_key")
    );
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, SECRET.as_bytes());
    let wanted = hex(ring::hmac::sign(&key, plain.as_bytes()).as_ref());
    if field("signature") != wanted || field("app_key") != APP_KEY {
        return Reply::json(200, &json!({"code": 401, "msg": "bad signature"}));
    }
    Reply::json(
        200,
        &json!({"code": 0, "data": {"token": TOKEN, "bot_id": BOT, "source": "bot"}}),
    )
}

fn rest(state: &Mutex<State>, base: &str, request: &Request) -> Reply {
    let mut s = state.lock().unwrap();
    let route = request.route().to_owned();
    match (request.method.as_str(), route.as_str()) {
        ("GET", "/") => Reply {
            status: 200,
            headers: vec![("date".into(), "Sat, 21 Nov 2026 08:49:37 GMT".into())],
            body: Vec::new(),
        },
        ("POST", "/api/v5/robotLogic/sign-token") => sign_token(request),
        ("POST", "/api/resource/genUploadInfo") => {
            let name = request.json()["fileName"]
                .as_str()
                .unwrap_or("f")
                .to_owned();
            let key = format!("up/{}/{name}", s.store.len() + 1);
            Reply::json(
                200,
                &json!({"code": 0, "data": {
                    "bucketName": "bucket", "location": key, "encryptTmpSecretId": "id",
                    "encryptTmpSecretKey": "key", "encryptToken": "tok", "startTime": 1, "expiredTime": 2,
                    "resourceUrl": format!("{base}/cos/{key}"),
                }}),
            )
        }
        ("PUT", path) if path.starts_with("/cos/") => {
            if request.header("x-cos-security-token") != Some("tok") {
                return Reply::status(403);
            }
            s.store
                .push((format!("{base}{path}"), request.body.clone()));
            Reply::status(200)
        }
        ("GET", path) if path.starts_with("/media/") => {
            match s.media.iter().find(|(p, _)| p == path) {
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

/// The element's file, when it names one the store holds.
fn stored(s: &State, chat: &str, element: &Fields) -> Option<MockFile> {
    let content = element.message(2);
    let (url, kind, name) = match element.string(1).as_str() {
        "TIMImageElem" => (
            content.message(8).string(5),
            MediaKind::Image,
            "image".to_owned(),
        ),
        "TIMFileElem" => {
            let name = content.string(12);
            (content.string(10), kind_of_name(&name), name)
        }
        _ => return None,
    };
    let (_, bytes) = s.store.iter().find(|(u, _)| *u == url)?;
    Some(MockFile {
        chat_id: chat.to_owned(),
        thread_id: None,
        name,
        kind,
        bytes: bytes.clone(),
    })
}

fn refusal(s: &mut State) -> Option<(u64, u64)> {
    Some(match s.fail_next.take()? {
        Failure::RateLimited(_) => (0, 429),
        Failure::ServerError => (500, 0),
        Failure::Forbidden | Failure::Markup => (0, 403),
    })
}

fn send_message(s: &mut State, cmd: &str, data: &Fields) -> (u64, Pb) {
    if let Some((status, code)) = refusal(s) {
        return (status, Pb::default().int(1, code).string(2, "refused"));
    }
    let (prefix, field) = if cmd == "send_c2c_message" {
        ("direct", 5)
    } else {
        ("group", 6)
    };
    let chat = format!("{prefix}:{}", data.string(2));
    let elements: Vec<Fields> = data.all(field).iter().map(|b| Fields::parse(b)).collect();
    let text = elements
        .iter()
        .filter(|e| e.string(1) == "TIMTextElem")
        .map(|e| e.message(2).string(1))
        .collect::<String>();
    let files: Vec<MockFile> = elements
        .iter()
        .filter_map(|e| stored(s, &chat, e))
        .collect();
    s.files.extend(files);
    s.sent.push(MockSent {
        chat_id: chat,
        message_id: data.string(1),
        text,
        buttons: Vec::new(),
        thread_id: None,
    });
    (0, Pb::default())
}

fn members(s: &State) -> Pb {
    s.members
        .iter()
        .fold(Pb::default(), |answer, (id, nickname)| {
            answer.message(3, &Pb::default().string(1, id).string(2, nickname))
        })
}

fn on_frame(state: &Mutex<State>, socket: &Socket, text: &str) {
    let raw = base64::engine::general_purpose::STANDARD
        .decode(text)
        .unwrap();
    let conn = Fields::parse(&raw);
    let head = conn.message(1);
    let data = conn.message(2);
    let (cmd, msg_id) = (head.string(2), head.string(4));
    let mut s = state.lock().unwrap();
    let (status, body) = match cmd.as_str() {
        "auth-bind" => {
            let ok = data.message(2).string(3) == TOKEN;
            s.bound = ok;
            if ok {
                for queued in s.queued.drain(..) {
                    socket.send_binary(queued);
                }
            }
            (if ok { 0 } else { 401 }, Pb::default())
        }
        "send_c2c_message" | "send_group_message" => send_message(&mut s, &cmd, &data),
        "send_private_heartbeat" => {
            s.beats += usize::from(data.int(3) == 1);
            return;
        }
        "get_group_member_list" => (0, members(&s)),
        _ => return,
    };
    socket.send_binary(frame(1, &cmd, &msg_id, status, &body));
}

impl Yuanbao {
    async fn start() -> Arc<Self> {
        let state = Arc::new(Mutex::new(State::default()));
        let (on_frames, for_rest) = (Arc::clone(&state), Arc::clone(&state));
        let base = Arc::new(Mutex::new(String::new()));
        let rest_base = Arc::clone(&base);
        let server = WsServer::start(
            Arc::new(|_: &Socket| true),
            Arc::new(move |socket: &Socket, frame: &str| on_frame(&on_frames, socket, frame)),
            Arc::new(move |request: &Request| {
                let base = rest_base.lock().unwrap().clone();
                rest(&for_rest, &base, request)
            }),
        )
        .await;
        *base.lock().unwrap() = server.base();
        Arc::new(Self {
            server,
            state,
            next: AtomicU64::new(1),
        })
    }

    /// Delivers an inbound push from `from` (in `group` when not empty).
    fn push(&self, from: &str, group: &str, body: &[Pb]) -> String {
        let id = format!("in{}", self.next.fetch_add(1, Ordering::Relaxed));
        let mut push = Pb::default()
            .string(
                1,
                if group.is_empty() {
                    "C2CMessage"
                } else {
                    "GroupMessage"
                },
            )
            .string(2, from)
            .string(4, "Alice")
            .string(12, &id);
        if !group.is_empty() {
            push = push.string(6, group);
        }
        for element in body {
            push = push.message(13, element);
        }
        let bytes = frame(2, "inbound_message", &id, 0, &push);
        let mut s = self.state.lock().unwrap();
        s.last = Some(bytes.clone());
        if !(s.bound && self.server.broadcast_binary(&bytes)) {
            s.queued.push(bytes);
        }
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
            "app_id": APP_KEY,
            "api_domain": self.server.base(),
            "ws_url": format!("{}/wss/connection", self.server.ws()),
            "cos_base": format!("{}/cos", self.server.base()),
            "media_url_prefixes": [format!("{}/media/", self.server.base())],
            "sidecar": bridge.sidecar,
        });
        for (k, v) in extra.as_object().into_iter().flatten() {
            options[k] = v.clone();
        }
        let hosts = [
            self.server.host(),
            bridge.base.trim_start_matches("http://").to_owned(),
        ];
        let (kit, root) = catalog::install("yuanbao", &options, &hosts);
        catalog::secret(&kit, "YUANBAO_APP_SECRET", SECRET);
        (kit, root)
    }
}

impl MockPlatform for Yuanbao {
    fn dm(&self) -> (String, String) {
        (format!("direct:{USER}"), USER.to_owned())
    }

    fn inject(&self, text: &str) -> String {
        self.push(USER, "", &[text_element(text)])
    }

    fn redeliver(&self) {
        let last = self.state.lock().unwrap().last.clone();
        if let Some(bytes) = last {
            self.server.broadcast_binary(&bytes);
        }
    }

    fn drop_connection(&self) {
        self.state.lock().unwrap().bound = false;
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
        self.state.lock().unwrap().beats
    }

    fn reactions(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    /// The caption and one element pointing at the mock's media CDN.
    fn inject_media(
        &self,
        kind: MediaKind,
        name: &str,
        bytes: &[u8],
        caption: &str,
    ) -> Option<String> {
        let path = {
            let mut s = self.state.lock().unwrap();
            let path = format!("/media/{}", s.media.len() + 1);
            s.media.push((path.clone(), bytes.to_vec()));
            path
        };
        let url = format!("{}{path}", self.server.base());
        let file = match kind {
            MediaKind::Image => element(
                "TIMImageElem",
                &Pb::default().message(8, &Pb::default().int(1, 1).string(5, &url)),
            ),
            MediaKind::Voice => element("TIMSoundElem", &Pb::default().string(10, &url)),
            MediaKind::Video => element("TIMVideoFileElem", &Pb::default().string(10, &url)),
            _ => element(
                "TIMFileElem",
                &Pb::default().string(10, &url).string(12, name),
            ),
        };
        Some(self.push(USER, "", &[text_element(caption), file]))
    }

    fn files(&self) -> Vec<MockFile> {
        self.state.lock().unwrap().files.clone()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_yuanbao_adapter_passes_the_conformance_suite() {
    let yb = Yuanbao::start().await;
    let bridge = Bridge::start(&[yb.server.host()]);
    let (kit, _root) = yb.install(&bridge, &json!({}));
    let report = kit
        .run(
            kit.adapter().unwrap(),
            Arc::clone(&yb) as Arc<dyn MockPlatform>,
        )
        .await;
    catalog::assert_passed(&report);
}

/// One `ForwardMsg` of a bundle.
fn forwarded(sender: &str, text: &str) -> Pb {
    let content = Pb::default().int(1, 1).string(2, text);
    Pb::default().string(1, sender).message(4, &content)
}

/// Scenario: Forwarded history decoded.
#[tokio::test(flavor = "multi_thread")]
async fn a_forwarded_bundle_reaches_the_agent_as_its_senders_lines_in_order() {
    let yb = Yuanbao::start().await;
    let bridge = Bridge::start(&[yb.server.host()]);
    let (kit, _root) = yb.install(&bridge, &json!({}));
    let adapter = kit.adapter().unwrap();
    let link = catalog::Link::new();
    adapter.connect(link.dyn_link()).await.unwrap();
    let lines = [
        ("ann", "the build is red"),
        ("bob", "which job?"),
        ("ann", "macos"),
        ("cy", "flaky test"),
        ("bob", "rerun it"),
    ];
    let bundle = lines
        .iter()
        .fold(Pb::default().int(1, 1), |bundle, (who, text)| {
            bundle.message(5, &forwarded(who, text))
        });
    let entry = Pb::default().string(1, "wexin_forward_msg_1_alice").string(
        2,
        &base64::engine::general_purpose::STANDARD.encode(&bundle.0),
    );
    yb.push(
        USER,
        "",
        &[
            element("TIMCustomElem", &Pb::default().message(999, &entry)),
            text_element("what happened here?"),
        ],
    );
    let admitted = link.wait_for(1).await;
    assert_eq!(admitted.len(), 1);
    let text = &admitted[0].text;
    let expected: Vec<String> = lines.iter().map(|(w, t)| format!("{w}: {t}")).collect();
    assert!(text.contains(&expected.join("\n")), "{text}");
    assert!(text.contains("what happened here?"), "{text}");
    adapter.disconnect().await;
}

fn tools(
    kit: &prometheus::plugins_cmd::adapter_plugin::PluginKit,
) -> Vec<pmt_tools::RegisteredTool> {
    let config: pmt_config::schema::PlatformConfig =
        serde_json::from_value(json!({"enabled": true})).unwrap();
    let secrets: SecretReader = Arc::new(|_| None);
    let context = Context {
        profile_dir: kit.setup().scope.dir(),
        config: &config,
        granted: None,
        secrets,
        network: &pmt_net::http::OutboundPolicy::default(),
    };
    prometheus::platform_tools_host::yuanbao::Tools.tools(&context)
}

async fn call(
    tools: &[pmt_tools::RegisteredTool],
    name: &str,
    args: Value,
) -> Result<String, String> {
    let tool = tools.iter().find(|t| t.name() == name).unwrap();
    let pmt_tools::descriptor::Executor::Handler(handler) = &tool.executor else {
        panic!("not a handler")
    };
    let context = pmt_tools::descriptor::CallContext {
        call_id: "c".into(),
        session_id: "s".into(),
        cancel: pmt_core::cancel::CancelToken::new(),
    };
    handler
        .call(&context, args)
        .await
        .map(|r| serde_json::to_string(&r.content).unwrap())
        .map_err(|f| f.message)
}

/// Scenario: Sticker search bounded.
#[tokio::test(flavor = "multi_thread")]
async fn a_sticker_search_over_fifty_fails_validation_naming_the_maximum() {
    let yb = Yuanbao::start().await;
    let bridge = Bridge::start(&[yb.server.host()]);
    let (kit, _root) = yb.install(&bridge, &json!({}));
    let all = tools(&kit);
    let names: Vec<&str> = all.iter().map(pmt_tools::RegisteredTool::name).collect();
    assert_eq!(
        names,
        [
            "yb_query_group_info",
            "yb_query_group_members",
            "yb_send_dm",
            "yb_search_sticker",
            "yb_send_sticker"
        ]
    );
    let search = all
        .iter()
        .find(|t| t.name() == "yb_search_sticker")
        .unwrap();
    let violations = pmt_tools::args::validate(
        &search.descriptor.parameters,
        &json!({"query": "like", "limit": 200}),
    );
    let text: Vec<String> = violations.iter().map(ToString::to_string).collect();
    assert!(text.iter().any(|v| v.contains("50")), "{text:?}");
    // The handler refuses it too, before the adapter is asked.
    let err = call(
        &all,
        "yb_search_sticker",
        json!({"query": "like", "limit": 200}),
    )
    .await
    .unwrap_err();
    assert!(err.contains("50"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_toolset_acts_over_the_connected_adapter() {
    let yb = Yuanbao::start().await;
    yb.state.lock().unwrap().members = vec![
        ("u-dan".into(), "Dan Brown".into()),
        ("u-eve".into(), "Eve".into()),
    ];
    let bridge = Bridge::start(&[yb.server.host()]);
    let (kit, _root) = yb.install(&bridge, &json!({}));
    let all = tools(&kit);
    let err = call(&all, "yb_search_sticker", json!({"query": "点赞"}))
        .await
        .unwrap_err();
    assert!(err.contains("not connected"), "{err}");
    let adapter = kit.adapter().unwrap();
    adapter
        .connect(catalog::Link::new().dyn_link())
        .await
        .unwrap();
    let found = call(
        &all,
        "yb_search_sticker",
        json!({"query": "点赞", "limit": 3}),
    )
    .await
    .unwrap();
    assert!(found.contains("点赞"), "{found}");
    call(
        &all,
        "yb_send_dm",
        json!({"name": "dan", "text": "standup moved", "group_code": "G1"}),
    )
    .await
    .unwrap();
    let sent = yb.sent();
    assert_eq!(sent.last().unwrap().chat_id, "direct:u-dan");
    assert_eq!(sent.last().unwrap().text, "standup moved");
    adapter.disconnect().await;
}

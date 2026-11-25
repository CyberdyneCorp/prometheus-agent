//! The `photon` catalog adapter plugin (spec: platform-adapters, Photon
//! iMessage adapter; Catalog adapter set). Its real sidecar (`photon.mjs`
//! on the socket bridge) runs against a stand-in for the Spectrum SDK: a
//! `spectrum-ts` package placed next to a copy of the sidecar, which
//! talks to the mock Photon here. The conformance suite, attachments as
//! metadata markers, polls and read receipts.

#![cfg(feature = "gateway")]

mod common;

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use common::catalog::{self, Bridge, MockHttp, Reply, Request};
use pmt_gateway::messaging::conformance::{
    Failure, MockFile, MockPlatform, MockSent, kind_of_name,
};
use pmt_gateway::messaging::{Button, MediaKind, Outgoing};
use serde_json::{Value, json};

const PROJECT: &str = "proj-1";
const SECRET: &str = "proj-secret";
const PHONE: &str = "+15551234567";
const DM: &str = "any;-;+15551234567";

/// The Spectrum SDK stand-in: the API surface the sidecar uses, backed by
/// the mock's `/login`, `/next` (a long poll of inbound messages),
/// `/send` and `/read`.
const FAKE_INDEX: &str = r#"
import { readFileSync } from "node:fs";
const base = process.env.FAKE_SPECTRUM_URL;
export async function post(path, body) {
  const res = await fetch(base + path, { method: "POST", body: JSON.stringify(body) });
  const answer = await res.json();
  if (!res.ok) throw Object.assign(new Error(answer.error || "refused"), { status: res.status });
  return answer;
}
export function space(id, type = "dm") {
  return { id, type, send: (builder) => post("/send", { spaceId: id, builder }) };
}
export async function Spectrum(options) {
  await post("/login", options);
  return {
    messages: {
      async *[Symbol.asyncIterator]() {
        for (;;) {
          for (const m of await post("/next", {})) {
            yield [space(m.space.id, m.space.type), {
              id: m.id, direction: "inbound", sender: { id: m.sender }, content: m.content,
              timestamp: m.timestamp, read: () => post("/read", { id: m.id }),
            }];
          }
        }
      },
    },
  };
}
export const text = (t) => ({ type: "text", text: t });
export const markdown = (t) => ({ type: "markdown", text: t });
export const typing = (state) => ({ type: "typing", state });
export const poll = (title, options) => ({ type: "poll", title, options });
const file = (type) => (path, o) => ({ type, name: o?.name, mimeType: o?.mimeType, data: readFileSync(path).toString("base64") });
export const attachment = file("attachment");
export const voice = file("voice");
"#;

const FAKE_IMESSAGE: &str = r#"
import { space } from "../index.mjs";
export const imessage = Object.assign(
  () => ({ space: { create: async (phone) => space(phone), get: async (id) => space(id) } }),
  { config: () => ({}), effect: { message: { slam: "impact" } } },
);
export const effect = (builder, id) => ({ ...builder, effect: id });
"#;

#[derive(Default)]
struct State {
    queued: Vec<Value>,
    last: Option<Value>,
    logged_in: bool,
    sent: Vec<MockSent>,
    files: Vec<MockFile>,
    typing: usize,
    reads: Vec<String>,
    next: u64,
    fail_next: Option<Failure>,
}

struct Photon {
    api: MockHttp,
    state: Arc<Mutex<State>>,
    /// The sidecar copy with the stand-in SDK.
    sidecar_dir: tempfile::TempDir,
}

fn next(state: &Mutex<State>) -> Reply {
    for _ in 0..20 {
        let batch: Vec<Value> = state.lock().unwrap().queued.drain(..).collect();
        if !batch.is_empty() {
            return Reply::json(200, &json!(batch));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Reply::json(200, &json!([]))
}

fn send(s: &mut State, body: &Value) -> Reply {
    let builder = &body["builder"];
    let chat = body["spaceId"].as_str().unwrap_or_default().to_owned();
    if builder["type"] == "typing" {
        s.typing += 1;
        return Reply::json(200, &json!({}));
    }
    match s.fail_next.take() {
        Some(Failure::RateLimited(_)) => return Reply::json(429, &json!({"error": "slow down"})),
        Some(Failure::ServerError) => return Reply::json(503, &json!({"error": "unavailable"})),
        Some(Failure::Forbidden | Failure::Markup) => {
            return Reply::json(
                403,
                &json!({"error": "Target not allowed for this project"}),
            );
        }
        None => {}
    }
    s.next += 1;
    let id = format!("out{}", s.next);
    let text = match builder["type"].as_str().unwrap_or_default() {
        "poll" => {
            let options: Vec<String> = builder["options"]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
                .map(|(i, o)| format!("{}. {}", i + 1, o.as_str().unwrap_or_default()))
                .collect();
            format!(
                "{}\n{}",
                builder["title"].as_str().unwrap_or_default(),
                options.join("\n")
            )
        }
        kind @ ("attachment" | "voice") => {
            let name = builder["name"].as_str().unwrap_or_default().to_owned();
            s.files.push(MockFile {
                chat_id: chat.clone(),
                thread_id: None,
                kind: if kind == "voice" {
                    MediaKind::Voice
                } else {
                    kind_of_name(&name)
                },
                name,
                bytes: base64::engine::general_purpose::STANDARD
                    .decode(builder["data"].as_str().unwrap_or_default())
                    .unwrap(),
            });
            String::new()
        }
        _ => builder["text"].as_str().unwrap_or_default().to_owned(),
    };
    s.sent.push(MockSent {
        chat_id: chat,
        message_id: id.clone(),
        text,
        buttons: Vec::new(),
        thread_id: None,
    });
    Reply::json(200, &json!({"id": id}))
}

fn answer(state: &Mutex<State>, request: &Request) -> Reply {
    if request.route() == "/next" {
        return next(state);
    }
    let body = request.json();
    let mut s = state.lock().unwrap();
    match request.route() {
        "/login" => {
            s.logged_in = body["projectId"] == PROJECT && body["projectSecret"] == SECRET;
            Reply::json(if s.logged_in { 200 } else { 401 }, &json!({}))
        }
        "/send" => send(&mut s, &body),
        "/read" => {
            s.reads
                .push(body["id"].as_str().unwrap_or_default().to_owned());
            Reply::json(200, &json!({}))
        }
        _ => Reply::status(404),
    }
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

impl Photon {
    fn start() -> Arc<Self> {
        let state = Arc::new(Mutex::new(State::default()));
        let served = Arc::clone(&state);
        let api = MockHttp::start(Arc::new(move |request: &Request| answer(&served, request)));
        let sidecar_dir = catalog::scratch();
        let source =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/first-party/photon/sidecar");
        for file in ["photon.mjs", "bridge.mjs", "package.json"] {
            std::fs::copy(source.join(file), sidecar_dir.path().join(file)).unwrap();
        }
        let module = sidecar_dir.path().join("node_modules/spectrum-ts");
        write(&module.join("index.mjs"), FAKE_INDEX);
        write(&module.join("providers/imessage.mjs"), FAKE_IMESSAGE);
        write(
            &module.join("package.json"),
            &json!({"name": "spectrum-ts", "type": "module", "exports": {
                ".": "./index.mjs", "./providers/imessage": "./providers/imessage.mjs"}})
            .to_string(),
        );
        Arc::new(Self {
            api,
            state,
            sidecar_dir,
        })
    }

    fn sidecar(&self) -> Bridge {
        let script = self.sidecar_dir.path().join("photon.mjs");
        let base = self.api.base().to_owned();
        let bridge = Bridge::start_script_with_env(
            &script.display().to_string(),
            &["127.0.0.1".to_owned()],
            &[
                ("PHOTON_PROJECT_ID", PROJECT),
                ("PHOTON_PROJECT_SECRET", SECRET),
                ("FAKE_SPECTRUM_URL", &base),
                ("TMPDIR", &self.sidecar_dir.path().display().to_string()),
            ],
        );
        for _ in 0..100 {
            if self.state.lock().unwrap().logged_in {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        bridge
    }

    fn install(
        bridge: &Bridge,
        extra: &Value,
    ) -> (
        prometheus::plugins_cmd::adapter_plugin::PluginKit,
        tempfile::TempDir,
    ) {
        let mut options = json!({"sidecar": bridge.sidecar});
        for (k, v) in extra.as_object().into_iter().flatten() {
            options[k] = v.clone();
        }
        catalog::install(
            "photon",
            &options,
            &[bridge.base.trim_start_matches("http://").to_owned()],
        )
    }

    fn deliver(&self, content: &Value) -> String {
        let mut s = self.state.lock().unwrap();
        s.next += 1;
        let id = format!("in{}", s.next);
        let message = json!({"id": id, "space": {"id": DM, "type": "dm"}, "sender": PHONE,
            "content": content, "timestamp": "2026-11-21T08:00:00Z"});
        s.last = Some(message.clone());
        s.queued.push(message);
        id
    }
}

impl MockPlatform for Photon {
    fn dm(&self) -> (String, String) {
        (DM.to_owned(), PHONE.to_owned())
    }

    fn inject(&self, text: &str) -> String {
        self.deliver(&json!({"type": "text", "text": text}))
    }

    fn redeliver(&self) {
        let mut s = self.state.lock().unwrap();
        if let Some(last) = s.last.clone() {
            s.queued.push(last);
        }
    }

    /// Spectrum's own reconnects are the SDK's; nothing is lost meanwhile.
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
        Vec::new()
    }

    fn files(&self) -> Vec<MockFile> {
        self.state.lock().unwrap().files.clone()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_photon_adapter_passes_the_conformance_suite() {
    let photon = Photon::start();
    let bridge = photon.sidecar();
    let (kit, _root) = Photon::install(&bridge, &json!({}));
    let report = kit
        .run(
            kit.adapter().unwrap(),
            Arc::clone(&photon) as Arc<dyn MockPlatform>,
        )
        .await;
    catalog::assert_passed(&report);
}

/// Scenario: Inbound attachment metadata.
#[tokio::test(flavor = "multi_thread")]
async fn a_photo_arrives_as_a_marker_naming_the_file_and_type_without_bytes() {
    let photon = Photon::start();
    let bridge = photon.sidecar();
    let (kit, _root) = Photon::install(&bridge, &json!({}));
    let adapter = kit.adapter().unwrap();
    let link = catalog::Link::new();
    adapter.connect(link.dyn_link()).await.unwrap();
    let id = photon.deliver(&json!({
        "type": "attachment", "name": "IMG_0042.heic", "mimeType": "image/heic", "size": 48213,
        "data": base64::engine::general_purpose::STANDARD.encode(b"heic bytes"),
    }));
    let admitted = link.wait_for(1).await;
    assert_eq!(admitted.len(), 1);
    assert_eq!(admitted[0].text, "[attachment: IMG_0042.heic (image/heic)]");
    assert!(
        admitted[0].attachments.is_empty(),
        "no file bytes reach the agent"
    );
    // Admitted messages are marked read.
    for _ in 0..40 {
        if photon.state.lock().unwrap().reads.contains(&id) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(photon.state.lock().unwrap().reads, [id]);
    adapter.disconnect().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn choices_go_as_a_native_poll_and_a_vote_picks_one_without_read_receipts() {
    let photon = Photon::start();
    let bridge = photon.sidecar();
    let (kit, _root) = Photon::install(&bridge, &json!({"read_receipts": false}));
    let adapter = kit.adapter().unwrap();
    let link = catalog::Link::new();
    adapter.connect(link.dyn_link()).await.unwrap();
    let mut ask = Outgoing::text(DM, None, "Deploy now?");
    ask.buttons = vec![
        Button {
            label: "Yes".into(),
            data: "deploy:yes".into(),
        },
        Button {
            label: "No".into(),
            data: "deploy:no".into(),
        },
    ];
    adapter.send(&ask).await.unwrap();
    assert_eq!(photon.sent()[0].text, "Deploy now?\n1. Yes\n2. No");
    photon.deliver(&json!({"type": "poll_option", "option": {"title": "No"}, "selected": true}));
    let picked = link.wait_for(1).await;
    assert_eq!(picked[0].callback.as_deref(), Some("deploy:no"));
    adapter
        .send(&Outgoing::text(DM, None, "[effect:slam] Shipped"))
        .await
        .unwrap();
    assert_eq!(photon.sent().last().unwrap().text, "Shipped");
    assert!(photon.state.lock().unwrap().reads.is_empty());
    adapter.disconnect().await;
}

/// The mock Photon dashboard and Spectrum API of the setup command:
/// device login (pending once, then approved), projects, the secret,
/// users. Counts the device-code requests.
fn photon_api(codes: Arc<Mutex<usize>>, users: Arc<Mutex<Vec<Value>>>) -> MockHttp {
    let polls = Arc::new(Mutex::new(0usize));
    MockHttp::start(Arc::new(move |request: &Request| {
        let bearer = request.header("authorization") == Some("Bearer dash-token");
        let basic = request.header("authorization")
            == Some(&format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode("p1:s3cret")
            ));
        match (request.method.as_str(), request.route()) {
            ("POST", "/api/auth/device/code") => {
                *codes.lock().unwrap() += 1;
                Reply::json(
                    200,
                    &json!({"device_code": "dc", "user_code": "ABCD-EFGH",
                    "verification_uri": "https://photon.example/device", "interval": 0, "expires_in": 30}),
                )
            }
            ("POST", "/api/auth/device/token") => {
                let mut n = polls.lock().unwrap();
                *n += 1;
                if *n == 1 {
                    Reply::json(400, &json!({"error": "authorization_pending"}))
                } else {
                    Reply::json(200, &json!({"access_token": "dash-token"}))
                }
            }
            (_, path) if path.starts_with("/api/") && !bearer => {
                Reply::json(401, &json!({"error": "unauthorized"}))
            }
            ("GET", "/api/projects") => Reply::json(200, &json!({"data": []})),
            ("POST", "/api/projects") => Reply::json(
                200,
                &json!({"data": {"id": "p1", "name": "Prometheus Agent"}}),
            ),
            ("POST", "/api/projects/p1/regenerate-secret") => {
                Reply::json(200, &json!({"projectSecret": "s3cret"}))
            }
            (_, "/projects/p1/users/") if !basic => {
                Reply::json(401, &json!({"error": "bad credentials"}))
            }
            ("GET", "/projects/p1/users/") => {
                Reply::json(200, &json!({"users": *users.lock().unwrap()}))
            }
            ("POST", "/projects/p1/users/") => {
                let user = json!({"phoneNumber": request.json()["phoneNumber"], "assignedPhoneNumber": "+18885550000"});
                users.lock().unwrap().push(user.clone());
                Reply::json(200, &json!({"user": user}))
            }
            _ => Reply::status(404),
        }
    }))
}

#[test]
fn setup_signs_in_provisions_registers_and_a_rerun_reuses_everything() {
    let (codes, users) = (Arc::new(Mutex::new(0)), Arc::new(Mutex::new(Vec::new())));
    let api = photon_api(Arc::clone(&codes), Arc::clone(&users));
    // The photon plugin installed and enabled, pointed at the mock.
    let options = json!({"dashboard_url": api.base(), "spectrum_url": api.base()});
    let (_kit, root) = catalog::install("photon", &options, &[]);
    let home = root.path().join("home");
    let args = [
        "gateway",
        "photon",
        "setup",
        "--phone",
        "+15551234567",
        "--no-install",
    ];
    let first = common::ok(&common::run(&home, &args));
    assert!(first.contains("ABCD-EFGH"), "{first}");
    assert!(first.contains("created the Photon project"), "{first}");
    assert!(first.contains("registered the phone number"), "{first}");
    assert!(
        first.contains("Text +18885550000 from +15551234567"),
        "{first}"
    );
    for secret in ["dash-token", "s3cret"] {
        assert!(!first.contains(secret), "a secret was printed: {first}");
    }
    let listed = common::ok(&common::run(&home, &["secrets", "list"]));
    for name in [
        "PHOTON_DASHBOARD_TOKEN",
        "PHOTON_PROJECT_ID",
        "PHOTON_PROJECT_SECRET",
    ] {
        assert!(listed.contains(name), "{listed}");
    }
    let again = common::ok(&common::run(&home, &args));
    assert!(
        again.contains("reusing the stored Photon sign-in"),
        "{again}"
    );
    assert!(again.contains("stored project secret is valid"), "{again}");
    assert!(again.contains("already registered"), "{again}");
    assert_eq!(*codes.lock().unwrap(), 1, "no second device login");
    assert_eq!(users.lock().unwrap().len(), 1);
    let status = common::ok(&common::run(&home, &["gateway", "photon", "status"]));
    assert!(
        status.contains("number +15551234567: texts +18885550000"),
        "{status}"
    );
    let bad = common::run(&home, &["gateway", "photon", "setup", "--phone", "5551234"]);
    assert!(!bad.status.success());
}

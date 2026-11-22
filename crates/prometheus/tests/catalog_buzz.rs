//! The `buzz` catalog adapter plugin (spec: platform-adapters, Buzz Nostr
//! adapter; Catalog adapter set) against a mock Buzz relay (NIP-42 `AUTH`,
//! `REQ` replays from `since`, live `EVENT`s; held by the sidecar's socket
//! bridge) and a fake Buzz CLI the real sidecar runs: the conformance
//! suite, the private key passed by environment only, mention gating,
//! the poll fallback and the SHA-256 check of inbound media.

#![cfg(feature = "gateway")]

mod common;

use std::os::unix::fs::PermissionsExt as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use common::catalog::{self, Bridge, Reply, Request};
use common::catalog_ws::{Socket, WsServer};
use pmt_gateway::messaging::MediaKind;
use pmt_gateway::messaging::conformance::{
    Failure, MockFile, MockPlatform, MockSent, kind_of_name,
};
use serde_json::{Value, json};

/// NIP-19's example key pair: the bot.
const BOT_KEY: &str = "nsec1vl029mgpspedva04g90vltkh6fvh240zqtv9k0t9af8935ke9laqsnlfe5";
const BOT_PK: &str = "7e7e9c42a91bfef19fa929e5fda1b72e0ebc1a4c1141673e2794234d86addf4e";
const USER_PK: &str = "a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";
const DM: &str = "dm-alice";
const TOWN: &str = "town";
/// The relay's clock (its `Date` header).
const DATE: &str = "Sat, 21 Nov 2026 08:49:37 GMT";
const T0: u64 = 1_795_250_977;

/// The fake Buzz CLI: forwards each run to the mock (`/cli`), with its
/// argv, stdin, the key it found in its environment and any `--file`.
const FAKE_CLI: &str = r#"#!/usr/bin/env node
import fs from "node:fs";
const args = process.argv.slice(2);
const input = fs.readFileSync(0, "utf8");
const at = args.indexOf("--file");
const file = at >= 0 ? fs.readFileSync(args[at + 1]).toString("base64") : null;
const name = at >= 0 ? args[at + 1].split("/").pop() : null;
const base = (process.env.BUZZ_RELAY_URL || "").replace(/^ws/, "http");
const res = await fetch(base + "/cli", {
  method: "POST",
  body: JSON.stringify({ args, input, key: process.env.BUZZ_PRIVATE_KEY || "", file, name }),
});
const body = await res.json();
if (body.error) {
  process.stderr.write(body.error);
  process.exit(1);
}
process.stdout.write(JSON.stringify(body.out));
"#;

#[derive(Default)]
struct State {
    events: Vec<Value>,
    authed: bool,
    sent: Vec<MockSent>,
    files: Vec<MockFile>,
    media: Vec<(String, Vec<u8>)>,
    /// Every CLI run: (argv, key in its environment).
    runs: Vec<(Vec<String>, String)>,
    refuse_sockets: bool,
    fail_next: Option<Failure>,
}

struct Relay {
    server: WsServer,
    state: Arc<Mutex<State>>,
    next: AtomicU64,
    _dir: tempfile::TempDir,
    cli: String,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut out, b| {
        use std::fmt::Write as _;
        let _ = write!(out, "{b:02x}");
        out
    })
}

fn sha256(bytes: &[u8]) -> String {
    hex(ring::digest::digest(&ring::digest::SHA256, bytes).as_ref())
}

/// Whether `event` matches a `REQ` filter.
fn matches(filter: &Value, event: &Value) -> bool {
    let tagged = |name: &str| -> Vec<String> {
        event["tags"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|t| t[0] == name)
            .filter_map(|t| t[1].as_str().map(str::to_owned))
            .collect()
    };
    let wants = |key: &str, tag: &str| {
        filter[key].as_array().is_none_or(|want| {
            want.iter()
                .filter_map(Value::as_str)
                .any(|w| tagged(tag).iter().any(|t| t == w))
        })
    };
    event["created_at"].as_u64() >= filter["since"].as_u64()
        && filter["kinds"]
            .as_array()
            .is_none_or(|k| k.contains(&event["kind"]))
        && wants("#h", "h")
        && wants("#p", "p")
}

fn auth_ok(event: &Value) -> bool {
    let canonical = json!([
        0,
        event["pubkey"],
        event["created_at"],
        22242,
        event["tags"],
        ""
    ])
    .to_string();
    event["kind"] == 22242
        && event["pubkey"] == BOT_PK
        && event["id"] == sha256(canonical.as_bytes())
        && event["tags"]
            .as_array()
            .is_some_and(|t| t.contains(&json!(["challenge", "chal-1"])))
}

fn on_frame(state: &Mutex<State>, socket: &Socket, frame: &str) {
    let message: Value = serde_json::from_str(frame).unwrap();
    let mut s = state.lock().unwrap();
    match message[0].as_str() {
        Some("AUTH") => {
            let ok = auth_ok(&message[1]);
            s.authed = ok;
            socket.send(
                json!([
                    "OK",
                    message[1]["id"],
                    ok,
                    if ok { "" } else { "auth-required: bad" }
                ])
                .to_string(),
            );
        }
        Some("REQ") if s.authed => {
            let sub = message[1].clone();
            for event in s.events.iter().filter(|e| matches(&message[2], e)) {
                socket.send(json!(["EVENT", sub, event]).to_string());
            }
            socket.send(json!(["EOSE", sub]).to_string());
        }
        _ => {}
    }
}

fn arg<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

fn cli(s: &mut State, run: &Value) -> Reply {
    let args: Vec<String> = run["args"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| a.as_str().map(str::to_owned))
        .collect();
    s.runs.push((
        args.clone(),
        run["key"].as_str().unwrap_or_default().to_owned(),
    ));
    if args.first().map(String::as_str) == Some("messages")
        && args.get(1).map(String::as_str) == Some("get")
    {
        let channel = arg(&args, "--channel").unwrap_or_default().to_owned();
        let since: u64 = arg(&args, "--since")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let filter = json!({"#h": [channel], "since": since});
        let found: Vec<Value> = s
            .events
            .iter()
            .filter(|e| matches(&filter, e))
            .cloned()
            .collect();
        return Reply::json(200, &json!({"out": found}));
    }
    match s.fail_next.take() {
        Some(Failure::RateLimited(_)) => {
            return Reply::json(200, &json!({"error": "Error: rate limit exceeded"}));
        }
        Some(Failure::ServerError) => {
            return Reply::json(200, &json!({"error": "relay unavailable (503)"}));
        }
        Some(Failure::Forbidden | Failure::Markup) => {
            return Reply::json(200, &json!({"error": "not a member of this channel"}));
        }
        None => {}
    }
    let chat = arg(&args, "--channel").unwrap_or_default().to_owned();
    let thread = arg(&args, "--reply-to").map(str::to_owned);
    let id = format!("out{}", s.sent.len() + 1);
    if let Some(data) = run["file"].as_str() {
        let name = run["name"].as_str().unwrap_or_default().to_owned();
        s.files.push(MockFile {
            chat_id: chat.clone(),
            thread_id: thread.clone(),
            kind: kind_of_name(&name),
            name,
            bytes: base64::engine::general_purpose::STANDARD
                .decode(data)
                .unwrap(),
        });
    }
    s.sent.push(MockSent {
        chat_id: chat,
        message_id: id.clone(),
        text: run["input"].as_str().unwrap_or_default().to_owned(),
        buttons: Vec::new(),
        thread_id: thread,
    });
    Reply::json(200, &json!({"out": {"id": id}}))
}

fn rest(state: &Mutex<State>, request: &Request) -> Reply {
    let mut s = state.lock().unwrap();
    match (request.method.as_str(), request.route()) {
        ("GET", "/") => Reply {
            status: 200,
            headers: vec![("date".into(), DATE.into())],
            body: Vec::new(),
        },
        ("POST", "/cli") => cli(&mut s, &request.json()),
        ("GET", path) => match s.media.iter().find(|(p, _)| p == path) {
            Some((_, bytes)) => Reply {
                status: 200,
                headers: Vec::new(),
                body: bytes.clone(),
            },
            None => Reply::status(404),
        },
        _ => Reply::status(404),
    }
}

impl Relay {
    async fn start() -> Arc<Self> {
        let state = Arc::new(Mutex::new(State::default()));
        let (on_open, on_frames, for_rest) =
            (Arc::clone(&state), Arc::clone(&state), Arc::clone(&state));
        let server = WsServer::start(
            Arc::new(move |socket: &Socket| {
                let mut s = on_open.lock().unwrap();
                if s.refuse_sockets {
                    return false;
                }
                s.authed = false;
                socket.send(json!(["AUTH", "chal-1"]).to_string());
                true
            }),
            Arc::new(move |socket: &Socket, frame: &str| on_frame(&on_frames, socket, frame)),
            Arc::new(move |request: &Request| rest(&for_rest, request)),
        )
        .await;
        let dir = catalog::scratch();
        let cli = dir.path().join("fake-buzz.mjs");
        std::fs::write(&cli, FAKE_CLI).unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
        Arc::new(Self {
            server,
            state,
            next: AtomicU64::new(1),
            cli: cli.display().to_string(),
            _dir: dir,
        })
    }

    /// Publishes an event from the user in `channel` (p-tagging the bot
    /// in a DM); its id.
    fn publish(&self, channel: &str, content: &str, extra_tags: &[Value]) -> String {
        let n = self.next.fetch_add(1, Ordering::Relaxed);
        let mut tags = vec![json!(["h", channel])];
        if channel == DM {
            tags.push(json!(["p", BOT_PK]));
        }
        tags.extend_from_slice(extra_tags);
        let id = sha256(format!("event-{n}-{content}").as_bytes());
        let event = json!({
            "id": id, "pubkey": USER_PK, "created_at": T0 + n, "kind": 9,
            "tags": tags, "content": content, "sig": "00",
        });
        let mut s = self.state.lock().unwrap();
        s.events.push(event.clone());
        if s.authed {
            self.server
                .broadcast(&json!(["EVENT", "live", event]).to_string());
        }
        id
    }

    fn sidecar(&self) -> Bridge {
        Bridge::start_script_with_env(
            "plugins/first-party/buzz/sidecar/buzz.mjs",
            &[self.server.host()],
            &[("BUZZ_PRIVATE_KEY", BOT_KEY)],
        )
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
            "relay_url": self.server.ws(),
            "channels": [TOWN],
            "name": "prom",
            "cli_path": self.cli,
            "sidecar": bridge.sidecar,
        });
        for (k, v) in extra.as_object().into_iter().flatten() {
            options[k] = v.clone();
        }
        let hosts = [
            self.server.host(),
            bridge.base.trim_start_matches("http://").to_owned(),
        ];
        let (mut kit, root) = catalog::install("buzz", &options, &hosts);
        catalog::secret(&kit, "BUZZ_PRIVATE_KEY", BOT_KEY);
        kit.allow_users(&[USER_PK]);
        (kit, root)
    }
}

impl MockPlatform for Relay {
    fn dm(&self) -> (String, String) {
        (DM.to_owned(), USER_PK.to_owned())
    }

    fn inject(&self, text: &str) -> String {
        self.publish(DM, text, &[])
    }

    fn redeliver(&self) {
        let last = self.state.lock().unwrap().events.last().cloned();
        if let Some(event) = last {
            self.server
                .broadcast(&json!(["EVENT", "live", event]).to_string());
        }
    }

    fn drop_connection(&self) {
        self.state.lock().unwrap().authed = false;
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

    fn thread(&self) -> Option<(String, String)> {
        Some((TOWN.to_owned(), "root-event".to_owned()))
    }

    /// A DM whose `imeta` names a file on the relay's own origin.
    fn inject_media(
        &self,
        _kind: MediaKind,
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
        let imeta = json!([
            "imeta",
            format!("url {}{path}", self.server.base()),
            format!("x {}", sha256(bytes)),
            format!("size {}", bytes.len()),
            format!("filename {name}"),
        ]);
        Some(self.publish(DM, caption, &[imeta]))
    }

    fn files(&self) -> Vec<MockFile> {
        self.state.lock().unwrap().files.clone()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_buzz_adapter_passes_the_conformance_suite() {
    let relay = Relay::start().await;
    let bridge = relay.sidecar();
    let (kit, _root) = relay.install(&bridge, &json!({}));
    let report = kit
        .run(
            kit.adapter().unwrap(),
            Arc::clone(&relay) as Arc<dyn MockPlatform>,
        )
        .await;
    catalog::assert_passed(&report);
    // Every CLI run had the key in its environment and none on argv.
    let runs = relay.state.lock().unwrap().runs.clone();
    assert!(!runs.is_empty());
    for (args, key) in runs {
        assert_eq!(key, BOT_KEY);
        assert!(
            args.iter()
                .all(|a| !a.contains(BOT_KEY) && !a.contains("nsec1")),
            "{args:?}"
        );
    }
}

/// Scenario: Media hash mismatch.
#[tokio::test(flavor = "multi_thread")]
async fn an_attachment_whose_digest_differs_is_discarded_and_the_agent_told() {
    let relay = Relay::start().await;
    relay
        .state
        .lock()
        .unwrap()
        .media
        .push(("/media/evil".into(), b"tampered".to_vec()));
    let bridge = relay.sidecar();
    let (kit, _root) = relay.install(&bridge, &json!({}));
    let adapter = kit.adapter().unwrap();
    let link = catalog::Link::new();
    adapter.connect(link.dyn_link()).await.unwrap();
    let imeta = json!([
        "imeta",
        format!("url {}/media/evil", relay.server.base()),
        format!("x {}", sha256(b"the original")),
        "size 8",
        "filename report.pdf",
    ]);
    relay.publish(DM, "see attached", &[imeta]);
    let admitted = link.wait_for(1).await;
    assert_eq!(admitted.len(), 1);
    assert!(admitted[0].attachments.is_empty());
    assert!(
        admitted[0].text.contains("report.pdf failed verification"),
        "{}",
        admitted[0].text
    );
    adapter.disconnect().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn channels_need_a_mention_and_history_is_not_replayed() {
    let relay = Relay::start().await;
    // Before the adapter connects: history it must not replay.
    relay.state.lock().unwrap().events.push(json!({
        "id": "old", "pubkey": USER_PK, "created_at": T0 - 100, "kind": 9,
        "tags": [["h", TOWN]], "content": "@prom old news", "sig": "00",
    }));
    let bridge = relay.sidecar();
    let (kit, _root) = relay.install(&bridge, &json!({}));
    let adapter = kit.adapter().unwrap();
    let link = catalog::Link::new();
    adapter.connect(link.dyn_link()).await.unwrap();
    relay.publish(TOWN, "just chatting", &[]);
    relay.publish(TOWN, "@prom what is the status?", &[]);
    // Events arrive in order: had the unmentioned one (or the old one)
    // been admitted, it would come first.
    let admitted = link.wait_for(1).await;
    let texts: Vec<&str> = admitted.iter().map(|i| i.text.as_str()).collect();
    assert_eq!(texts, ["@prom what is the status?"]);
    adapter.disconnect().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unreachable_relay_falls_back_to_polling_the_cli() {
    let relay = Relay::start().await;
    relay.state.lock().unwrap().refuse_sockets = true;
    let bridge = relay.sidecar();
    let (kit, _root) = relay.install(&bridge, &json!({"poll_interval_secs": 1}));
    let adapter = kit.adapter().unwrap();
    let link = catalog::Link::new();
    adapter.connect(link.dyn_link()).await.unwrap();
    relay.publish(TOWN, "@prom are you there?", &[]);
    let admitted = link.wait_for(1).await;
    assert_eq!(admitted.len(), 1, "polled through the CLI");
    let runs = relay.state.lock().unwrap().runs.clone();
    assert!(
        runs.iter()
            .any(|(args, _)| args[..2] == ["messages", "get"])
    );
    adapter.disconnect().await;
}

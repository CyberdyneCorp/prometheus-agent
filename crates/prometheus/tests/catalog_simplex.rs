//! The `simplex` catalog adapter plugin (spec: platform-adapters, `SimpleX`
//! adapter; Catalog adapter set) against a mock `simplex-chat` daemon
//! WebSocket, held by the bridge sidecar (system Node): the conformance
//! suite (files in through the daemon's file transfer and the plugin's
//! `fs_read` grant, files out by path), contact requests accepted, groups
//! ignored unless allowed, and numeric contact ids enforced.

#![cfg(feature = "gateway")]

mod common;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use common::catalog::{self, Bridge, Reply, Request};
use common::catalog_ws::{Socket, WsServer};
use pmt_gateway::messaging::MediaKind;
use pmt_gateway::messaging::Outgoing;
use pmt_gateway::messaging::conformance::{
    Failure, MockFile, MockPlatform, MockSent, kind_of_name,
};
use serde_json::{Value, json};

const CONTACT: u64 = 7;

#[derive(Default)]
struct State {
    queued: Vec<String>,
    last: Option<String>,
    sent: Vec<MockSent>,
    files_out: Vec<MockFile>,
    accepted: Vec<u64>,
    /// Files offered to the bot: id -> (name, bytes).
    offered: Vec<(u64, String, Vec<u8>)>,
}

struct Daemon {
    server: WsServer,
    state: Arc<Mutex<State>>,
    files: tempfile::TempDir,
    next: AtomicU64,
}

fn direct(contact: u64) -> Value {
    json!({"type": "direct", "contact": {"contactId": contact, "localDisplayName": "alice"}})
}

fn item(
    info: &Value,
    id: u64,
    content: &Value,
    file: Option<(u64, &str)>,
    member: Option<u64>,
) -> String {
    let mut chat_item = json!({
        "meta": {"itemId": id},
        "content": {"type": "rcvMsgContent", "msgContent": content},
    });
    if let Some((file_id, name)) = file {
        chat_item["file"] = json!({"fileId": file_id, "fileName": name, "fileSize": 10});
    }
    if let Some(member) = member {
        chat_item["chatDir"] =
            json!({"groupMember": {"memberContactId": member, "groupMemberId": 99}});
    }
    json!({"resp": {"type": "newChatItems", "chatItems": [{"chatInfo": info, "chatItem": chat_item}]}}).to_string()
}

/// The daemon's answer to one command.
fn command(state: &Mutex<State>, files: &std::path::Path, socket: &Socket, frame: &str) {
    let request: Value = serde_json::from_str(frame).unwrap_or_default();
    let (corr, cmd) = (
        request["corrId"].clone(),
        request["cmd"].as_str().unwrap_or_default(),
    );
    let mut state = state.lock().unwrap();
    if let Some(id) = cmd.strip_prefix("/_accept ") {
        state.accepted.push(id.parse().unwrap_or_default());
    } else if let Some(file) = cmd.strip_prefix("/freceive ") {
        let id: u64 = file.parse().unwrap_or_default();
        if let Some((_, name, bytes)) = state.offered.iter().find(|(f, ..)| *f == id) {
            let path = files.join(name);
            std::fs::write(&path, bytes).unwrap();
            socket.send(
                json!({"resp": {"type": "rcvFileComplete", "chatItem": {"chatItem": {"file": {
                    "fileId": id, "fileName": name, "fileSource": {"filePath": path},
                }}}}})
                .to_string(),
            );
        }
    } else if let Some(rest) = cmd.strip_prefix("/_send ") {
        let (to, items) = rest.split_once(" json ").unwrap_or_default();
        let contact = to.trim_start_matches(['@', '#']);
        let items: Value = serde_json::from_str(items).unwrap_or_default();
        let id = 1000 + state.sent.len() as u64;
        for entry in items.as_array().into_iter().flatten() {
            let content = &entry["msgContent"];
            match entry["filePath"].as_str() {
                Some(path) => {
                    let name = PathBuf::from(path)
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned();
                    let kind = match content["type"].as_str() {
                        Some("voice") => MediaKind::Voice,
                        Some("file") => MediaKind::Document,
                        _ => kind_of_name(&name),
                    };
                    let bytes = std::fs::read(path).unwrap_or_default();
                    state.files_out.push(MockFile {
                        chat_id: contact.to_owned(),
                        thread_id: None,
                        name,
                        kind,
                        bytes,
                    });
                }
                None => state.sent.push(MockSent {
                    chat_id: contact.to_owned(),
                    message_id: id.to_string(),
                    text: content["text"].as_str().unwrap_or_default().to_owned(),
                    buttons: Vec::new(),
                    thread_id: None,
                }),
            }
        }
        socket.send(
            json!({"corrId": corr, "resp": {"type": "newChatItems", "chatItems": [{"chatItem": {"meta": {"itemId": id}}}]}})
                .to_string(),
        );
    }
}

impl Daemon {
    async fn start() -> Arc<Self> {
        let state = Arc::new(Mutex::new(State::default()));
        let files = catalog::scratch();
        let (on_open, on_frame, dir) = (
            Arc::clone(&state),
            Arc::clone(&state),
            files.path().to_path_buf(),
        );
        let server = WsServer::start(
            Arc::new(move |socket: &Socket| {
                for frame in on_open.lock().unwrap().queued.drain(..) {
                    socket.send(frame);
                }
                true
            }),
            Arc::new(move |socket: &Socket, frame: &str| command(&on_frame, &dir, socket, frame)),
            Arc::new(|_: &Request| Reply::status(404)),
        )
        .await;
        Arc::new(Self {
            server,
            state,
            files,
            next: AtomicU64::new(1),
        })
    }

    fn deliver(&self, frame: String) {
        self.state.lock().unwrap().last = Some(frame.clone());
        if !self.server.broadcast(&frame) {
            self.state.lock().unwrap().queued.push(frame);
        }
    }

    fn id(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed)
    }

    fn install(
        &self,
        bridge: &Bridge,
        extra: &Value,
    ) -> (
        prometheus::plugins_cmd::adapter_plugin::PluginKit,
        tempfile::TempDir,
    ) {
        let mut options = json!({"ws_url": self.server.ws(), "sidecar": bridge.sidecar});
        for (k, v) in extra.as_object().into_iter().flatten() {
            options[k] = v.clone();
        }
        let folder = std::fs::canonicalize(self.files.path()).unwrap();
        catalog::install_with(
            "simplex",
            &options,
            &[bridge.base.trim_start_matches("http://").to_owned()],
            |manifest| {
                let capabilities = manifest
                    .entry("capabilities")
                    .or_insert_with(|| toml::Value::Table(toml::Table::new()))
                    .as_table_mut()
                    .unwrap();
                capabilities.insert(
                    "fs_read".into(),
                    toml::Value::Array(vec![format!("{}/**", folder.display()).into()]),
                );
            },
        )
    }
}

impl MockPlatform for Daemon {
    fn dm(&self) -> (String, String) {
        (CONTACT.to_string(), CONTACT.to_string())
    }

    fn inject(&self, text: &str) -> String {
        let id = self.id();
        self.deliver(item(
            &direct(CONTACT),
            id,
            &json!({"type": "text", "text": text}),
            None,
            None,
        ));
        id.to_string()
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

    fn fail_next(&self, _failure: Failure) {}

    fn reports_markup_errors(&self) -> bool {
        false
    }

    /// The local daemon queues every message it accepts; a refusal is a
    /// command error, never a rate limit or a server failure.
    fn sends_can_fail(&self) -> bool {
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

    fn inject_media(
        &self,
        kind: MediaKind,
        name: &str,
        bytes: &[u8],
        caption: &str,
    ) -> Option<String> {
        let (id, file) = (self.id(), self.id());
        self.state
            .lock()
            .unwrap()
            .offered
            .push((file, name.to_owned(), bytes.to_vec()));
        let kind = match kind {
            MediaKind::Image => "image",
            MediaKind::Video => "video",
            MediaKind::Voice => "voice",
            _ => "file",
        };
        let content = json!({"type": kind, "text": caption});
        self.deliver(item(
            &direct(CONTACT),
            id,
            &content,
            Some((file, name)),
            None,
        ));
        Some(id.to_string())
    }

    fn files(&self) -> Vec<MockFile> {
        self.state.lock().unwrap().files_out.clone()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_simplex_adapter_passes_the_conformance_suite() {
    let daemon = Daemon::start().await;
    let bridge = Bridge::start(&[daemon.server.host()]);
    let (kit, _root) = daemon.install(&bridge, &json!({}));
    let report = kit
        .run(
            kit.adapter().unwrap(),
            Arc::clone(&daemon) as Arc<dyn MockPlatform>,
        )
        .await;
    catalog::assert_passed(&report);
}

#[tokio::test(flavor = "multi_thread")]
async fn contact_requests_are_accepted_and_only_allowed_groups_are_heard() {
    let daemon = Daemon::start().await;
    let bridge = Bridge::start(&[daemon.server.host()]);
    let (kit, _root) = daemon.install(&bridge, &json!({"allow_groups": ["5"]}));
    let adapter = kit.adapter().unwrap();
    let link = catalog::Link::new();
    adapter.connect(link.dyn_link()).await.unwrap();
    daemon.deliver(json!({"resp": {"type": "receivedContactRequest", "contactRequest": {"contactRequestId": 31}}}).to_string());
    let text = |t: &str| json!({"type": "text", "text": t});
    let group = |g: u64| json!({"type": "group", "groupInfo": {"groupId": g}});
    daemon.deliver(item(
        &group(6),
        daemon.id(),
        &text("not for the bot"),
        None,
        Some(8),
    ));
    daemon.deliver(item(
        &group(5),
        daemon.id(),
        &text("hello group"),
        None,
        Some(8),
    ));
    let admitted = link.wait_for(1).await;
    assert_eq!(admitted.len(), 1, "{admitted:?}");
    assert_eq!(admitted[0].chat_id, "group:5");
    assert_eq!(admitted[0].sender_id, "8");
    assert_eq!(daemon.state.lock().unwrap().accepted, [31]);
    adapter
        .send(&Outgoing::text("group:5", None, "hi all"))
        .await
        .unwrap();
    assert_eq!(daemon.sent()[0].chat_id, "5");
    adapter.disconnect().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_display_name_allowlist_stops_the_adapter() {
    let daemon = Daemon::start().await;
    let bridge = Bridge::start(&[daemon.server.host()]);
    let (mut kit, _root) = daemon.install(&bridge, &json!({}));
    kit.allow_users(&["alice"]);
    let err = kit
        .adapter()
        .unwrap()
        .connect(catalog::Link::new().dyn_link())
        .await
        .unwrap_err();
    assert!(
        err.message.contains("numeric contact ids"),
        "{}",
        err.message
    );
}

/// A file the daemon saved outside the plugin's `fs_read` grant is not
/// read: the message arrives without it.
#[tokio::test(flavor = "multi_thread")]
async fn a_file_outside_the_grant_is_not_read() {
    let daemon = Daemon::start().await;
    let bridge = Bridge::start(&[daemon.server.host()]);
    let mut options = json!({"ws_url": daemon.server.ws(), "sidecar": bridge.sidecar});
    options["auto_accept"] = json!(false);
    let (kit, _root) = catalog::install(
        "simplex",
        &options,
        &[bridge.base.trim_start_matches("http://").to_owned()],
    );
    let adapter = kit.adapter().unwrap();
    let link = catalog::Link::new();
    adapter.connect(link.dyn_link()).await.unwrap();
    daemon.inject_media(
        MediaKind::Document,
        "secret.pdf",
        b"private bytes",
        "see file",
    );
    let admitted = link.wait_for(1).await;
    assert_eq!(admitted.len(), 1, "{admitted:?}");
    assert_eq!(admitted[0].text, "see file");
    assert!(
        admitted[0].attachments.is_empty(),
        "{:?}",
        admitted[0].attachments
    );
    adapter.disconnect().await;
}

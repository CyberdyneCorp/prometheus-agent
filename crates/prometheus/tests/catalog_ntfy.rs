//! The `ntfy` catalog adapter plugin (spec: platform-adapters, ntfy
//! adapter; Catalog adapter set) against a mock ntfy server, the topic's
//! JSON stream held by the socket bridge sidecar (run with the system
//! Node): the conformance suite, outgoing-only without an allowlist,
//! truncation with a notice, and `fatal` on 401 and 404.

#![cfg(feature = "gateway")]

mod common;

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::catalog::{self, Bridge};
use pmt_gateway::messaging::conformance::{Failure, MockPlatform, MockSent};
use pmt_gateway::messaging::{AdapterState, Outgoing};
use serde_json::{Value, json};

const TOPIC: &str = "kit-alerts";
const TOKEN: &str = "tk_kit";

#[derive(Default)]
struct State {
    /// Every message published to the subscribe topic: (id, body).
    log: Vec<(String, String)>,
    /// How many of them reached a subscriber.
    delivered: usize,
    streams: Vec<TcpStream>,
    subscriptions: usize,
    published: Vec<MockSent>,
    fail_next: Option<Failure>,
    /// Every request's status forced (401, 404), when set.
    refuse: Option<u16>,
}

/// One ntfy server.
struct Server {
    port: u16,
    state: Arc<Mutex<State>>,
}

fn event(id: &str, text: &str) -> String {
    let (title, text) = text
        .split_once('\u{1}')
        .map_or((None, text), |(t, m)| (Some(t), m));
    let mut event =
        json!({"id": id, "time": 1, "event": "message", "topic": TOPIC, "message": text});
    if let Some(title) = title {
        event["title"] = json!(title);
    }
    event.to_string() + "\n"
}

fn head(stream: &mut TcpStream, status: u16, length: Option<usize>) {
    let length = length.map_or(String::new(), |n| format!("content-length: {n}\r\n"));
    let _ = write!(
        stream,
        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\n{length}connection: close\r\n\r\n"
    );
}

fn subscribe(state: &Mutex<State>, mut stream: TcpStream, path: &str) {
    head(&mut stream, 200, None);
    let _ = stream.write_all(b"{\"event\":\"open\"}\n");
    let mut state = state.lock().unwrap();
    state.subscriptions += 1;
    let from = match path.split_once("since=") {
        Some((_, since)) => state
            .log
            .iter()
            .position(|(id, _)| id == since)
            .map_or(0, |i| i + 1),
        None => state.delivered,
    };
    for (id, text) in &state.log[from..] {
        let _ = stream.write_all(event(id, text).as_bytes());
    }
    state.delivered = state.log.len();
    state.streams.push(stream);
}

fn publish(
    state: &Mutex<State>,
    mut stream: TcpStream,
    path: &str,
    headers: &[String],
    body: &str,
) {
    let mut state = state.lock().unwrap();
    let (status, answer) = match state.fail_next.take() {
        Some(Failure::RateLimited(_)) => (429, json!({"code": 42901, "error": "limit"})),
        Some(Failure::ServerError) => (500, json!({})),
        Some(Failure::Forbidden | Failure::Markup) => (403, json!({"code": 40301})),
        None => {
            let id = format!("p{}", state.published.len() + 1);
            let markdown = headers.iter().any(|h| h == "markdown: yes");
            state.published.push(MockSent {
                chat_id: path.trim_start_matches('/').to_owned(),
                message_id: id.clone(),
                text: if markdown {
                    format!("[md]{body}")
                } else {
                    body.to_owned()
                },
                buttons: Vec::new(),
                thread_id: None,
            });
            (200, json!({"id": id, "event": "message"}))
        }
    };
    let body = answer.to_string();
    head(&mut stream, status, Some(body.len()));
    let _ = stream.write_all(body.as_bytes());
}

fn serve(state: &Mutex<State>, stream: TcpStream) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap_or_default();
    let mut words = line.split_whitespace();
    let (method, path) = (
        words.next().unwrap_or_default().to_owned(),
        words.next().unwrap_or_default().to_owned(),
    );
    let mut headers = Vec::new();
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).unwrap_or_default();
        let header = header.trim_end().to_ascii_lowercase();
        if header.is_empty() {
            break;
        }
        headers.push(header);
    }
    let length = headers
        .iter()
        .find_map(|h| h.strip_prefix("content-length: ")?.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0; length];
    let _ = reader.read_exact(&mut body);
    let mut stream = stream;
    let refused = state.lock().unwrap().refuse;
    let authorized = headers
        .iter()
        .any(|h| *h == format!("authorization: bearer {TOKEN}"));
    if let Some(status) = refused.or((!authorized).then_some(401)) {
        head(&mut stream, status, Some(0));
        return;
    }
    if method == "GET" && path.starts_with(&format!("/{TOPIC}/json")) {
        subscribe(state, stream, &path);
    } else if method == "POST" {
        publish(
            state,
            stream,
            &path,
            &headers,
            &String::from_utf8_lossy(&body),
        );
    } else {
        head(&mut stream, 404, Some(0));
    }
}

impl Server {
    fn start() -> Arc<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(Mutex::new(State::default()));
        let served = Arc::clone(&state);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let state = Arc::clone(&served);
                std::thread::spawn(move || serve(&state, stream));
            }
        });
        Arc::new(Self { port, state })
    }

    fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn options(&self, bridge: &Bridge) -> Value {
        json!({"topic": TOPIC, "server_url": self.base(), "sidecar": bridge.sidecar})
    }

    fn hosts(&self, bridge: &Bridge) -> Vec<String> {
        vec![
            bridge.base.trim_start_matches("http://").to_owned(),
            format!("127.0.0.1:{}", self.port),
        ]
    }

    /// Someone publishes `text` to the topic.
    fn publish(&self, id: &str, text: &str) {
        let mut state = self.state.lock().unwrap();
        state.log.push((id.to_owned(), text.to_owned()));
        if state.streams.is_empty() {
            return;
        }
        state.delivered = state.log.len();
        for stream in &mut state.streams {
            let _ = stream.write_all(event(id, text).as_bytes());
        }
    }

    /// Someone publishes `text` with a `title` (stored as `title\u{1}text`).
    fn publish_titled(&self, id: &str, text: &str, title: &str) {
        self.publish(id, &format!("{title}\u{1}{text}"));
    }

    fn subscriptions(&self) -> usize {
        self.state.lock().unwrap().subscriptions
    }
}

impl MockPlatform for Server {
    fn dm(&self) -> (String, String) {
        (TOPIC.to_owned(), TOPIC.to_owned())
    }

    fn inject(&self, text: &str) -> String {
        let id = format!("m{}", self.state.lock().unwrap().log.len() + 1);
        self.publish(&id, text);
        id
    }

    fn redeliver(&self) {
        let mut state = self.state.lock().unwrap();
        let Some((id, text)) = state.log.last().cloned() else {
            return;
        };
        for stream in &mut state.streams {
            let _ = stream.write_all(event(&id, &text).as_bytes());
        }
    }

    fn drop_connection(&self) {
        for stream in self.state.lock().unwrap().streams.drain(..) {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }

    fn press(&self, _message_id: &str, _data: &str) {}

    fn fail_next(&self, failure: Failure) {
        self.state.lock().unwrap().fail_next = Some(failure);
    }

    fn reports_markup_errors(&self) -> bool {
        false
    }

    fn sent(&self) -> Vec<MockSent> {
        self.state.lock().unwrap().published.clone()
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

fn kit(
    server: &Server,
    bridge: &Bridge,
    allow: &[&str],
) -> (
    prometheus::plugins_cmd::adapter_plugin::PluginKit,
    tempfile::TempDir,
) {
    let (mut kit, root) = catalog::install("ntfy", &server.options(bridge), &server.hosts(bridge));
    catalog::secret(&kit, "NTFY_TOKEN", TOKEN);
    kit.allow_users(allow);
    (kit, root)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_ntfy_adapter_passes_the_conformance_suite() {
    let server = Server::start();
    let bridge = Bridge::start(&[format!("127.0.0.1:{}", server.port)]);
    let (kit, _root) = kit(&server, &bridge, &[TOPIC]);
    let adapter = kit.adapter().unwrap();
    let report = kit
        .run(adapter, Arc::clone(&server) as Arc<dyn MockPlatform>)
        .await;
    catalog::assert_passed(&report);
}

/// Scenario: Outgoing-only target.
#[tokio::test(flavor = "multi_thread")]
async fn without_an_allowlist_ntfy_admits_nothing_but_still_publishes() {
    let server = Server::start();
    let bridge = Bridge::start(&[format!("127.0.0.1:{}", server.port)]);
    let (kit, _root) = kit(&server, &bridge, &[]);
    let adapter = kit.adapter().unwrap();
    let link = catalog::Link::new();
    adapter.connect(link.dyn_link()).await.unwrap();
    server.publish_titled("x1", "anyone can write here", "admin");
    assert!(
        link.wait_for(1).await.is_empty(),
        "admitted without an allowlist"
    );
    assert_eq!(
        server.subscriptions(),
        0,
        "an outgoing-only adapter subscribed"
    );
    // A cron delivery (or `gateway send`) still publishes, cut at 4096
    // characters with a notice.
    adapter
        .send(&Outgoing::text(TOPIC, None, &"y".repeat(5000)))
        .await
        .unwrap();
    let published = server.sent();
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].chat_id, TOPIC);
    assert_eq!(published[0].text.chars().count(), 4096);
    assert!(published[0].text.contains("[truncated"));
    adapter.disconnect().await;
}

async fn stopped_with(refuse: u16) -> Vec<(AdapterState, Option<String>)> {
    let server = Server::start();
    let bridge = Bridge::start(&[format!("127.0.0.1:{}", server.port)]);
    server.state.lock().unwrap().refuse = Some(refuse);
    let (kit, _root) = kit(&server, &bridge, &[TOPIC]);
    let adapter = kit.adapter().unwrap();
    let link = catalog::Link::new();
    adapter.connect(link.dyn_link()).await.unwrap();
    for _ in 0..200 {
        if !link.states().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    adapter.disconnect().await;
    link.states()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_subscription_stops_the_adapter_with_its_code() {
    assert_eq!(
        stopped_with(401).await,
        [(AdapterState::Fatal, Some("ntfy_unauthorized".to_owned()))]
    );
    assert_eq!(
        stopped_with(404).await,
        [(AdapterState::Fatal, Some("ntfy_topic_not_found".to_owned()))]
    );
}

/// A publisher's title is shown with the message but never names the
/// sender: the topic is the only identity.
#[tokio::test(flavor = "multi_thread")]
async fn a_publisher_title_is_content_never_identity() {
    let server = Server::start();
    let bridge = Bridge::start(&[format!("127.0.0.1:{}", server.port)]);
    let (kit, _root) = kit(&server, &bridge, &[TOPIC]);
    let adapter = kit.adapter().unwrap();
    let link = catalog::Link::new();
    adapter.connect(link.dyn_link()).await.unwrap();
    server.publish_titled("t1", "reboot the server", "admin");
    let admitted = link.wait_for(1).await;
    assert_eq!(admitted.len(), 1, "{admitted:?}");
    assert_eq!(admitted[0].sender_id, TOPIC);
    assert_eq!(admitted[0].text, "admin\nreboot the server");
    adapter.disconnect().await;
}

//! The `irc` catalog adapter plugin (spec: platform-adapters, IRC adapter;
//! Catalog adapter set) against a mock IRC server, its connection held by
//! the socket bridge sidecar (run with the system Node): the conformance
//! suite, registration and channel joins, mentions in channels, replies
//! split into `PRIVMSG` lines within 450 bytes and the credential lock
//! keyed by server and nickname.

#![cfg(feature = "gateway")]

mod common;

use std::io::{BufRead as _, BufReader, Write as _};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use common::catalog::{self, Bridge};
use pmt_gateway::messaging::Outgoing;
use pmt_gateway::messaging::conformance::{Failure, MockPlatform, MockSent};
use serde_json::{Value, json};

const NICK: &str = "pbot";
const USER: &str = "kituser";

#[derive(Default)]
struct State {
    stream: Option<TcpStream>,
    registered: bool,
    /// Lines for the bot while it is away (delivered once it is back).
    queued: Vec<String>,
    last: Option<String>,
    /// `PRIVMSG` lines the bot sent: (target, text).
    sent: Vec<(String, String)>,
    joined: Vec<String>,
    /// Every raw line the bot sent.
    raw: Vec<String>,
}

/// A one-client IRC server.
struct Server {
    port: u16,
    state: Arc<Mutex<State>>,
    next: AtomicU64,
}

fn welcome(state: &Mutex<State>, stream: &TcpStream) {
    let mut writer = stream.try_clone().unwrap();
    let _ = writer.write_all(format!(":mock 001 {NICK} :Welcome\r\n").as_bytes());
    let mut state = state.lock().unwrap();
    state.registered = true;
    for line in state.queued.drain(..) {
        let _ = writer.write_all(line.as_bytes());
    }
}

fn client(state: &Arc<Mutex<State>>, stream: &TcpStream) {
    state.lock().unwrap().stream = Some(stream.try_clone().unwrap());
    let reader = BufReader::new(stream.try_clone().unwrap());
    for line in reader.lines() {
        let Ok(line) = line else { break };
        state.lock().unwrap().raw.push(line.clone());
        let mut words = line.splitn(3, ' ');
        match (words.next(), words.next(), words.next()) {
            (Some("USER"), _, _) => welcome(state, stream),
            (Some("JOIN"), Some(channel), _) => {
                state.lock().unwrap().joined.push(channel.to_owned());
            }
            (Some("PRIVMSG"), Some(target), Some(text)) => {
                assert!(
                    line.len() + 2 <= 512,
                    "an IRC line is at most 512 bytes: {line}"
                );
                let text = text.strip_prefix(':').unwrap_or(text).to_owned();
                state.lock().unwrap().sent.push((target.to_owned(), text));
            }
            _ => {}
        }
    }
    let mut state = state.lock().unwrap();
    state.stream = None;
    state.registered = false;
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
                std::thread::spawn(move || client(&state, &stream));
            }
        });
        Arc::new(Self {
            port,
            state,
            next: AtomicU64::new(1),
        })
    }

    fn options(&self, bridge: &Bridge) -> Value {
        json!({
            "server": "127.0.0.1",
            "port": self.port,
            "use_tls": false,
            "nickname": NICK,
            "channels": ["#kit"],
            "sidecar": bridge.sidecar,
        })
    }

    /// Sends the bot a raw line now, or once it is registered again.
    fn line(&self, line: String) {
        let mut state = self.state.lock().unwrap();
        state.last = Some(line.clone());
        match (&state.stream, state.registered) {
            (Some(stream), true) => {
                let _ = stream.try_clone().unwrap().write_all(line.as_bytes());
            }
            _ => state.queued.push(line),
        }
    }

    fn wait<T>(&self, mut done: impl FnMut(&State) -> Option<T>) -> T {
        for _ in 0..300 {
            if let Some(found) = done(&self.state.lock().unwrap()) {
                return found;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        panic!(
            "the IRC server never saw it: {:?}",
            self.state.lock().unwrap().raw
        );
    }
}

impl MockPlatform for Server {
    fn dm(&self) -> (String, String) {
        (USER.to_owned(), USER.to_owned())
    }

    fn inject(&self, text: &str) -> String {
        self.line(format!(":{USER}!u@kit PRIVMSG {NICK} :{text}\r\n"));
        self.next.fetch_add(1, Ordering::Relaxed).to_string()
    }

    /// IRC never redelivers a message.
    fn redeliver(&self) {}

    fn drop_connection(&self) {
        let mut state = self.state.lock().unwrap();
        if let Some(stream) = state.stream.take() {
            let _ = stream.shutdown(Shutdown::Both);
        }
        state.registered = false;
    }

    fn press(&self, _message_id: &str, _data: &str) {}

    fn fail_next(&self, _failure: Failure) {}

    fn reports_markup_errors(&self) -> bool {
        false
    }

    fn sent(&self) -> Vec<MockSent> {
        self.state
            .lock()
            .unwrap()
            .sent
            .iter()
            .enumerate()
            .map(|(i, (target, text))| MockSent {
                chat_id: target.clone(),
                message_id: i.to_string(),
                text: text.clone(),
                buttons: Vec::new(),
                thread_id: None,
            })
            .collect()
    }

    fn typing(&self) -> usize {
        0
    }

    fn reactions(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    /// The server answers a flood or a bad target asynchronously, never
    /// as the result of a write: no send fails.
    fn sends_can_fail(&self) -> bool {
        false
    }
}

fn bridge_for(server: &Server) -> Bridge {
    Bridge::start(&[format!("127.0.0.1:{}", server.port)])
}

#[tokio::test(flavor = "multi_thread")]
async fn the_irc_adapter_passes_the_conformance_suite() {
    let server = Server::start();
    let bridge = bridge_for(&server);
    let report = catalog::conformance(
        "irc",
        &server.options(&bridge),
        &[bridge.base.trim_start_matches("http://").to_owned()],
        &[],
        Arc::clone(&server) as Arc<dyn MockPlatform>,
    )
    .await;
    catalog::assert_passed(&report);
}

/// Scenario: Long reply split.
#[tokio::test(flavor = "multi_thread")]
async fn a_long_reply_is_sent_as_privmsg_lines_within_450_bytes() {
    let server = Server::start();
    let bridge = bridge_for(&server);
    let (kit, _root) = catalog::install(
        "irc",
        &server.options(&bridge),
        &[bridge.base.trim_start_matches("http://").to_owned()],
    );
    catalog::secret(&kit, "IRC_NICKSERV_PASSWORD", "hunter2");
    let adapter = kit.adapter().unwrap();
    assert_eq!(
        adapter.credential_identity().as_deref(),
        Some("127.0.0.1|pbot")
    );
    let link = catalog::Link::new();
    adapter.connect(link.dyn_link()).await.unwrap();
    server.wait(|s| s.joined.contains(&"#kit".to_owned()).then_some(()));
    server.wait(|s| {
        s.raw
            .iter()
            .any(|l| l == "PRIVMSG NickServ :IDENTIFY hunter2")
            .then_some(())
    });
    // A channel message naming the bot is a mention, addressed text kept.
    server.line(format!(
        ":alice!a@kit PRIVMSG #kit :{NICK}: what is up?\r\n"
    ));
    let admitted = link.wait_for(1).await;
    assert_eq!(admitted[0].chat_id, "#kit");
    assert_eq!(admitted[0].text, "what is up?");
    assert!(admitted[0].addressing.mentioned);

    let reply = "word ".repeat(240); // 1200 bytes
    let mut sent = false;
    for _ in 0..100 {
        if adapter
            .send(&Outgoing::text("#kit", None, &reply))
            .await
            .is_ok()
        {
            sent = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(sent, "the adapter never registered");
    let lines = server.wait(|s| {
        let lines: Vec<String> = s
            .sent
            .iter()
            .filter(|(t, _)| t == "#kit")
            .map(|(_, text)| text.clone())
            .collect();
        (lines.concat().replace(' ', "").len() == reply.replace(' ', "").len()).then_some(lines)
    });
    assert!(lines.len() >= 3, "{lines:?}");
    assert!(lines.iter().all(|l| l.len() <= 450), "{lines:?}");
    adapter.disconnect().await;
}

/// A carriage return or NUL in the model's text (or in a chat name) never
/// ends the protocol line early: a server that ends lines on a bare CR
/// would otherwise run the rest as a raw command.
#[tokio::test(flavor = "multi_thread")]
async fn a_carriage_return_in_a_reply_starts_no_second_command() {
    let server = Server::start();
    let bridge = bridge_for(&server);
    let (kit, _root) = catalog::install(
        "irc",
        &server.options(&bridge),
        &[bridge.base.trim_start_matches("http://").to_owned()],
    );
    let adapter = kit.adapter().unwrap();
    adapter
        .connect(catalog::Link::new().dyn_link())
        .await
        .unwrap();
    server.wait(|s| s.joined.contains(&"#kit".to_owned()).then_some(()));
    let mut sent = false;
    for _ in 0..100 {
        if adapter
            .send(&Outgoing::text("#kit", None, "hi\rQUIT :x\0"))
            .await
            .is_ok()
        {
            sent = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(sent, "the adapter never registered");
    adapter
        .send(&Outgoing::text("#kit\rJOIN #evil", None, "x"))
        .await
        .unwrap();
    let raw = server.wait(|s| (s.sent.len() >= 3).then(|| s.raw.clone()));
    assert!(
        raw.iter().all(|l| !l.contains(['\r', '\0'])),
        "a protocol line held a CR or NUL: {raw:?}"
    );
    assert!(
        raw.iter()
            .all(|l| l.starts_with("PRIVMSG ") || !l.contains("QUIT :x")),
        "{raw:?}"
    );
    assert!(!raw.iter().any(|l| l.starts_with("JOIN #evil")), "{raw:?}");
    adapter.disconnect().await;
}

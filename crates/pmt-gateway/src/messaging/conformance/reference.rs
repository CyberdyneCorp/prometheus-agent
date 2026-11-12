//! The reference platform: the mock the conformance kit runs a
//! `platform-adapter` plugin against (`prometheus plugins test-adapter
//! <plugin dir>`). A plugin's real platform is unknown to the kit, so a
//! plugin under test speaks this small loopback HTTP API instead, at the
//! base URL the kit passes as its `api` option:
//!
//! - `GET /updates`: the messages delivered since the last poll, a JSON
//!   list of `{id, chat, chat_type, sender, text, callback?}` (the shape the
//!   host's `poll` expects, so a plugin may pass it through).
//! - `POST /send` with the host's outgoing JSON (`chat`, `thread`,
//!   `reply_to`, `text`, `buttons`): `200` with the message id as the body;
//!   `429` with `{"retry_after": <secs>}`, `5xx`, `400` (`{"error":
//!   "markup"}` or a message over the limit) and `403` are the failures the
//!   plugin classifies (WIT `send`).
//!
//! It enforces the limit and unit the plugin declares ([`Reference::enforce`]), and a dropped
//! connection is one `503` answer to the next poll.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::{Value, json};

use super::{Failure, MockPlatform, MockSent};
use crate::messaging::adapter::Button;
use crate::messaging::descriptor::LengthUnit;

const DM: &str = "kit-dm";
const USER: &str = "kit-user";

#[derive(Default)]
struct State {
    queued: Vec<Value>,
    last: Option<Value>,
    next_id: u64,
    drop_next_poll: bool,
    fail_next: Option<Failure>,
    sent: Vec<MockSent>,
    /// The limit and unit the adapter declares, once known.
    limit: Option<(usize, LengthUnit)>,
}

/// The reference platform server (see the module docs).
pub struct Reference {
    state: Arc<Mutex<State>>,
    base: String,
}

impl Reference {
    /// Starts it on a loopback port.
    ///
    /// # Errors
    ///
    /// No loopback port can be bound.
    pub fn start() -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let base = format!("http://{}", listener.local_addr()?);
        let state = Arc::new(Mutex::new(State::default()));
        let served = Arc::clone(&state);
        std::thread::Builder::new()
            .name("conformance-reference".to_owned())
            .spawn(move || {
                for stream in listener.incoming().flatten() {
                    let state = Arc::clone(&served);
                    std::thread::spawn(move || answer(&stream, &state));
                }
            })?;
        Ok(Self { state, base })
    }

    /// Refuses messages longer than `limit` `unit`s from now on (what the
    /// adapter's descriptor declares).
    pub fn enforce(&self, limit: usize, unit: LengthUnit) {
        self.state().limit = Some((limit, unit));
    }

    /// `http://127.0.0.1:<port>`: the plugin's `api` option.
    #[must_use]
    pub fn base(&self) -> &str {
        &self.base
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn deliver(&self, mut message: Value) -> String {
        let mut state = self.state();
        state.next_id += 1;
        let id = format!("in-{}", state.next_id);
        message["id"] = json!(id);
        state.queued.push(message.clone());
        state.last = Some(message);
        id
    }
}

fn inbound(text: &str) -> Value {
    json!({"chat": DM, "chat_type": "dm", "sender": USER, "text": text})
}

impl MockPlatform for Reference {
    fn dm(&self) -> (String, String) {
        (DM.to_owned(), USER.to_owned())
    }

    fn inject(&self, text: &str) -> String {
        self.deliver(inbound(text))
    }

    fn redeliver(&self) {
        let mut state = self.state();
        if let Some(last) = state.last.clone() {
            state.queued.push(last);
        }
    }

    fn drop_connection(&self) {
        self.state().drop_next_poll = true;
    }

    fn press(&self, _message_id: &str, data: &str) {
        let mut message = inbound("");
        message["callback"] = json!(data);
        self.deliver(message);
    }

    fn fail_next(&self, failure: Failure) {
        self.state().fail_next = Some(failure);
    }

    fn reports_markup_errors(&self) -> bool {
        true
    }

    fn sent(&self) -> Vec<MockSent> {
        self.state().sent.clone()
    }

    fn typing(&self) -> usize {
        0
    }

    fn reactions(&self) -> Vec<(String, String)> {
        Vec::new()
    }
}

/// One answer: status and body.
type Answer = (u16, String);

fn answer(stream: &TcpStream, state: &Mutex<State>) {
    let Some((line, body)) = read_request(stream) else {
        return;
    };
    let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
    let (status, body) = if line.starts_with("GET /updates") {
        poll(&mut state)
    } else if line.starts_with("POST /send") {
        send(&mut state, &body)
    } else {
        (404, String::new())
    };
    drop(state);
    let reason = if status < 300 { "OK" } else { "Error" };
    let _ = write!(
        &mut &*stream,
        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
}

fn poll(state: &mut State) -> Answer {
    if std::mem::take(&mut state.drop_next_poll) {
        return (503, String::new());
    }
    (
        200,
        Value::Array(std::mem::take(&mut state.queued)).to_string(),
    )
}

fn send(state: &mut State, body: &[u8]) -> Answer {
    match state.fail_next.take() {
        Some(Failure::RateLimited(secs)) => return (429, json!({"retry_after": secs}).to_string()),
        Some(Failure::ServerError) => return (500, String::new()),
        Some(Failure::Markup) => return (400, json!({"error": "markup"}).to_string()),
        Some(Failure::Forbidden) => return (403, String::new()),
        None => {}
    }
    let Ok(message) = serde_json::from_slice::<Value>(body) else {
        return (400, json!({"error": "not JSON"}).to_string());
    };
    let text = message["text"].as_str().unwrap_or_default().to_owned();
    if state
        .limit
        .is_some_and(|(limit, unit)| unit.len(&text) > limit)
    {
        return (400, json!({"error": "message too long"}).to_string());
    }
    state.next_id += 1;
    let id = format!("out-{}", state.next_id);
    state.sent.push(MockSent {
        chat_id: message["chat"].as_str().unwrap_or_default().to_owned(),
        message_id: id.clone(),
        text,
        buttons: serde_json::from_value::<Vec<Button>>(message["buttons"].clone())
            .unwrap_or_default(),
        thread_id: message["thread"].as_str().map(str::to_owned),
    });
    (200, id)
}

/// The request line and body of one HTTP/1.1 request.
fn read_request(stream: &TcpStream) -> Option<(String, Vec<u8>)> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut length = 0;
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).ok()?;
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).ok()?;
    Some((line, body))
}

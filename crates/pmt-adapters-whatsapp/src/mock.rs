//! A `WhatsApp` bridge mock: the queue `GET /messages` drains (messages wait
//! there while the adapter is away, as the bridge keeps them), sends of at
//! most 4,096 characters, edits, media, typing, chat info, a health state
//! that can be logged out, and scripted failures.

// A fixture: a mock that cannot serve should fail loudly.
#![allow(clippy::missing_panics_doc)]

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use percent_encoding::percent_decode_str;
use pmt_gateway::messaging::conformance::{Failure, MockPlatform, MockSent};
use pmt_gateway::messaging::mock_server;
use pmt_gateway::messaging::{HttpRequest, HttpResponse};
use serde_json::{Value, json};

/// The person the kit talks as (and their direct chat).
pub const USER: &str = "15550004242@s.whatsapp.net";
const LIMIT: usize = 4096;

#[derive(Default)]
struct State {
    queue: Vec<Value>,
    last: Option<Value>,
    next: u64,
    sent: Vec<MockSent>,
    media: Vec<(String, String)>,
    typing: usize,
    failures: VecDeque<Failure>,
    fail_next_fetch: bool,
    status: String,
}

/// The mock.
pub struct BridgeMock {
    state: Mutex<State>,
    addr: Mutex<Option<SocketAddr>>,
}

impl BridgeMock {
    /// Starts the bridge on a free loopback port.
    pub async fn start() -> Arc<Self> {
        let mock = Arc::new(Self {
            state: Mutex::new(State {
                next: 100,
                status: "connected".to_owned(),
                ..State::default()
            }),
            addr: Mutex::new(None),
        });
        let server = Arc::clone(&mock);
        let addr = mock_server::serve(move |request| {
            let server = Arc::clone(&server);
            async move { server.serve(&request) }
        })
        .await
        .unwrap();
        *mock.addr.lock().unwrap() = Some(addr);
        mock
    }

    /// The table lines that point the adapter at this bridge.
    pub fn table(&self) -> String {
        let addr = self.addr.lock().unwrap().expect("started");
        format!("options = {{ bridge_url = \"http://{addr}\" }}")
    }

    /// The linked device is logged out from the phone.
    pub fn log_out(&self) {
        let mut state = self.lock();
        "logged_out".clone_into(&mut state.status);
        state.fail_next_fetch = true;
    }

    /// `(chat, mediaType)` of the media sent.
    pub fn media(&self) -> Vec<(String, String)> {
        self.lock().media.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    /// Queues an inbound event with the given fields (a message id is
    /// added unless given); its message id.
    pub fn receive(&self, fields: &Value) -> String {
        let mut state = self.lock();
        state.next += 1;
        let mut event = json!({"messageId": format!("WA{}", state.next), "chatId": USER,
            "senderId": USER, "senderName": "Kit", "isGroup": false, "body": "",
            "timestamp": 1_700_000_000});
        for (key, value) in fields.as_object().into_iter().flatten() {
            event[key] = value.clone();
        }
        let id = event["messageId"].as_str().unwrap_or_default().to_owned();
        state.queue.push(event.clone());
        state.last = Some(event);
        id
    }

    fn serve(&self, request: &HttpRequest) -> HttpResponse {
        let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        let mut state = self.lock();
        match (request.method.as_str(), request.path.as_str()) {
            ("GET", "/health") => mock_server::json(200, &json!({"status": state.status})),
            ("GET", "/messages") => {
                if std::mem::take(&mut state.fail_next_fetch) {
                    return mock_server::json(503, &json!({"error": "Not connected"}));
                }
                mock_server::json(200, &Value::Array(std::mem::take(&mut state.queue)))
            }
            ("POST", "/send") => send(&mut state, &body),
            ("POST", "/edit") => {
                let id = body["messageId"].as_str().unwrap_or_default();
                let Some(message) = state.sent.iter_mut().find(|m| m.message_id == id) else {
                    return mock_server::json(404, &json!({"error": "unknown message"}));
                };
                body["message"]
                    .as_str()
                    .unwrap_or_default()
                    .clone_into(&mut message.text);
                mock_server::json(200, &json!({"success": true}))
            }
            ("POST", "/send-media") => {
                state.next += 1;
                let entry = (
                    body["chatId"].as_str().unwrap_or_default().to_owned(),
                    body["mediaType"].as_str().unwrap_or_default().to_owned(),
                );
                state.media.push(entry);
                mock_server::json(
                    200,
                    &json!({"success": true, "messageId": format!("WA{}", state.next)}),
                )
            }
            ("POST", "/typing") => {
                state.typing += 1;
                mock_server::json(200, &json!({"success": true}))
            }
            ("GET", path) if path.starts_with("/chat/") => {
                let id = percent_decode_str(&path["/chat/".len()..])
                    .decode_utf8_lossy()
                    .into_owned();
                mock_server::json(
                    200,
                    &json!({"name": "Kit", "isGroup": id.ends_with("@g.us")}),
                )
            }
            _ => mock_server::json(404, &json!({"error": "not found"})),
        }
    }
}

fn send(state: &mut State, body: &Value) -> HttpResponse {
    if let Some(failure) = state.failures.pop_front() {
        return match failure {
            Failure::RateLimited(secs) => mock_server::json(429, &json!({"error": "rate limited"}))
                .with_header("retry-after", &secs.to_string()),
            Failure::ServerError => mock_server::json(500, &json!({"error": "boom"})),
            Failure::Markup | Failure::Forbidden => {
                mock_server::json(400, &json!({"error": "not on WhatsApp"}))
            }
        };
    }
    let text = body["message"].as_str().unwrap_or_default();
    if text.chars().count() > LIMIT {
        return mock_server::json(400, &json!({"error": "message too long"}));
    }
    state.next += 1;
    let id = format!("WA{}", state.next);
    state.sent.push(MockSent {
        chat_id: body["chatId"].as_str().unwrap_or_default().to_owned(),
        message_id: id.clone(),
        text: text.to_owned(),
        buttons: Vec::new(),
    });
    mock_server::json(
        200,
        &json!({"success": true, "messageId": id, "messageIds": [id]}),
    )
}

impl MockPlatform for BridgeMock {
    fn dm(&self) -> (String, String) {
        (USER.to_owned(), USER.to_owned())
    }

    fn inject(&self, text: &str) -> String {
        self.receive(&json!({"body": text}))
    }

    fn redeliver(&self) {
        let mut state = self.lock();
        if let Some(event) = state.last.clone() {
            state.queue.push(event);
        }
    }

    fn drop_connection(&self) {
        self.lock().fail_next_fetch = true;
    }

    fn press(&self, _message_id: &str, _data: &str) {}

    fn fail_next(&self, failure: Failure) {
        self.lock().failures.push_back(failure);
    }

    fn reports_markup_errors(&self) -> bool {
        false
    }

    fn sent(&self) -> Vec<MockSent> {
        self.lock().sent.clone()
    }

    fn typing(&self) -> usize {
        self.lock().typing
    }

    fn reactions(&self) -> Vec<(String, String)> {
        Vec::new()
    }
}

/// The adapter, built for a fresh profile of the home at `home` pointing
/// at the mock bridge, and the mock: what
/// `prometheus plugins test-adapter whatsapp` and the crate's tests run
/// the conformance kit on.
///
/// # Errors
///
/// The profile cannot be set up.
pub async fn kit(
    home: &std::path::Path,
) -> Result<
    (
        Arc<dyn pmt_gateway::messaging::Adapter>,
        Arc<dyn MockPlatform>,
    ),
    pmt_core::error::Error,
> {
    let mock = BridgeMock::start().await;
    let setup =
        pmt_gateway::messaging::conformance::kit_setup(home, crate::PLATFORM, &mock.table(), &[])?;
    let adapter = crate::Whatsapp::new(&setup)?;
    Ok((Arc::new(adapter), mock))
}

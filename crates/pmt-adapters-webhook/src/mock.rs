//! A webhook sender for the conformance kit: it posts GitHub-signed
//! deliveries of `{"text": ...}` to the route `kit` (whose prompt is
//! `{text}`), redelivers with the same delivery id, and reads the replies
//! the adapter recorded in the route's log.

// A fixture: a mock that cannot serve should fail loudly.
#![allow(clippy::missing_panics_doc)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use pmt_gateway::messaging::conformance::{Failure, MockPlatform, MockSent};
use serde_json::{Value, json};

/// The route the kit posts to.
pub const ROUTE: &str = "kit";
/// The secret holding the route's signing key.
pub const SECRET_NAME: &str = "KIT_WEBHOOK_SECRET";
/// The signing key.
pub const SECRET: &str = "kit-signing-key";
/// The route's table lines.
pub const TABLE: &str =
    "routes.kit = { secret_secret = \"KIT_WEBHOOK_SECRET\", prompt = \"{text}\" }";

/// The sender.
pub struct WebhookSender {
    base: Mutex<Option<String>>,
    client: reqwest::Client,
    last: Mutex<Option<(Vec<u8>, String)>>,
    next: AtomicU64,
    log: PathBuf,
}

impl WebhookSender {
    /// A sender whose adapter records replies in `log`.
    #[must_use]
    pub fn new(log: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            base: Mutex::new(None),
            client: pmt_net::http::client_builder(&pmt_net::OutboundPolicy::default())
                .and_then(|b| {
                    b.no_proxy().build().map_err(|e| {
                        pmt_core::error::Error::new(
                            &pmt_core::error::codes::NET_CLIENT_INIT,
                            e.to_string(),
                        )
                    })
                })
                .expect("an HTTP client"),
            last: Mutex::new(None),
            next: AtomicU64::new(1),
            log,
        })
    }

    fn post(&self, body: Vec<u8>, delivery: &str) {
        let Some(base) = self.base.lock().unwrap().clone() else {
            return;
        };
        let request = self
            .client
            .post(format!("{base}/{ROUTE}"))
            .header("content-type", "application/json")
            .header("x-github-event", "push")
            .header("x-github-delivery", delivery)
            .header(
                "x-hub-signature-256",
                crate::github_signature(SECRET, &body),
            )
            .body(body);
        tokio::spawn(async move {
            let _ = request.send().await;
        });
    }
}

impl MockPlatform for WebhookSender {
    fn dm(&self) -> (String, String) {
        (ROUTE.to_owned(), format!("webhook:{ROUTE}"))
    }

    fn inject(&self, text: &str) -> String {
        let delivery = format!("delivery-{}", self.next.fetch_add(1, Ordering::Relaxed));
        let body = json!({"text": text}).to_string().into_bytes();
        *self.last.lock().unwrap() = Some((body.clone(), delivery.clone()));
        self.post(body, &delivery);
        delivery
    }

    fn redeliver(&self) {
        let last = self.last.lock().unwrap().clone();
        if let Some((body, delivery)) = last {
            self.post(body, &delivery);
        }
    }

    fn drop_connection(&self) {}

    fn press(&self, _message_id: &str, _data: &str) {}

    fn fail_next(&self, _failure: Failure) {}

    fn reports_markup_errors(&self) -> bool {
        false
    }

    fn sent(&self) -> Vec<MockSent> {
        let text = std::fs::read_to_string(&self.log).unwrap_or_default();
        text.lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .map(|entry| MockSent {
                chat_id: entry["route"].as_str().unwrap_or_default().to_owned(),
                message_id: entry["message_id"].as_str().unwrap_or_default().to_owned(),
                text: entry["text"].as_str().unwrap_or_default().to_owned(),
                buttons: Vec::new(),
            })
            .collect()
    }

    fn typing(&self) -> usize {
        0
    }

    fn reactions(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    fn listen(&self, base: &str) {
        *self.base.lock().unwrap() = Some(base.to_owned());
    }

    fn sends_can_fail(&self) -> bool {
        false
    }
}

/// The adapter, built for a fresh profile of the home at `home` with the
/// route `kit`, and the sender posting to it: what
/// `prometheus plugins test-adapter webhook` and the crate's tests run the
/// conformance kit on.
///
/// # Errors
///
/// The profile cannot be set up.
#[allow(clippy::unused_async)] // one signature for every adapter's kit
pub async fn kit(
    home: &std::path::Path,
) -> Result<
    (
        Arc<dyn pmt_gateway::messaging::Adapter>,
        Arc<dyn MockPlatform>,
    ),
    pmt_core::error::Error,
> {
    let setup = pmt_gateway::messaging::conformance::kit_setup(
        home,
        crate::PLATFORM,
        TABLE,
        &[(SECRET_NAME, SECRET)],
    )?;
    let adapter = crate::Webhook::new(&setup);
    let sender = WebhookSender::new(adapter.log_file(ROUTE));
    Ok((Arc::new(adapter), sender))
}

//! Outbound webhooks (spec: plugin-system, Outbound webhooks): hook events
//! sent as `POST` requests to `[[hooks.outbound]]` targets.
//!
//! Firing only queues the delivery (a bounded queue per target, 1000
//! events; overflow is dropped with a warning), so a slow or dead endpoint
//! never delays the turn that fired it. A worker per target sends each
//! delivery with `X-Prometheus-Event`, `X-Prometheus-Delivery` and, when a
//! secret is named, `X-Prometheus-Signature-256: sha256=<HMAC-SHA256 hex
//! of the raw body>`; responses are ignored, a connection error or 5xx is
//! retried once after a pause, a 4xx is not, redirects are never
//! followed, and the destination passes the egress policy (loopback
//! targets, which config validation allows over plain HTTP, are the
//! user's own machine).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use pmt_config::ProfileScope;
use pmt_config::schema::OutboundHook;
use pmt_plugins::hooks::{ExternalHook, HookFuture, Payload};
use pmt_plugins::host::CallInfo;
use serde_json::{Value, json};
use tokio::sync::mpsc;

/// Deliveries queued per target before new ones are dropped.
pub const QUEUE: usize = 1000;
/// The pause before the one retry.
const RETRY_AFTER: Duration = Duration::from_secs(1);
/// Hooks whose matcher is read (the tool name).
const TOOL_EVENTS: &[&str] = &["pre_tool_call", "post_tool_call", "transform_tool_result"];

/// How long a closing session waits for its queued deliveries.
pub const FLUSH: Duration = Duration::from_secs(5);

/// Deliveries queued or in flight in this process.
static PENDING: AtomicUsize = AtomicUsize::new(0);

/// Waits until every queued delivery of this process was attempted, at
/// most `limit`: a short-lived process (a one-shot chat) calls it before
/// exiting so its last events still go out.
pub async fn flush(limit: Duration) {
    let until = tokio::time::Instant::now() + limit;
    while PENDING.load(Ordering::Acquire) > 0 && tokio::time::Instant::now() < until {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// One delivery: the raw body and its ids.
struct Delivery {
    event: String,
    id: String,
    body: Vec<u8>,
}

/// One `[[hooks.outbound]]` target.
pub struct Outbound {
    label: String,
    target: OutboundHook,
    matcher: Option<regex::Regex>,
    profile: String,
    secret: Option<String>,
    client: Option<reqwest::Client>,
    queue: OnceLock<Option<mpsc::Sender<Delivery>>>,
}

impl Outbound {
    /// A target of `scope`; its secret is read now.
    #[must_use]
    pub fn new(scope: &ProfileScope, target: OutboundHook) -> Self {
        let secret = target.secret_name.as_deref().and_then(|name| {
            scope
                .secrets()
                .resolve(name)
                .inspect_err(|err| tracing::warn!(webhook = %target.name, "webhook secret unavailable: {}", err.message()))
                .ok()
                .map(|s| s.expose().to_owned())
        });
        let client = client(scope, &target)
            .inspect_err(|why| tracing::warn!(webhook = %target.name, "webhook disabled: {why}"))
            .ok();
        Self {
            label: format!("webhook:{}", target.name),
            matcher: target
                .matcher
                .as_deref()
                .and_then(|m| regex::Regex::new(m).ok()),
            profile: scope.name().to_string(),
            target,
            secret,
            client,
            queue: OnceLock::new(),
        }
    }

    /// The queue, its worker started on first use (`None` outside a tokio
    /// runtime or without a client).
    fn queue(&self) -> Option<&mpsc::Sender<Delivery>> {
        self.queue
            .get_or_init(|| {
                let client = self.client.clone()?;
                let handle = tokio::runtime::Handle::try_current().ok()?;
                let (sender, receiver) = mpsc::channel(QUEUE);
                let worker = Worker {
                    name: self.target.name.clone(),
                    url: self.target.url.clone(),
                    secret: self.secret.clone(),
                    timeout: Duration::from_secs(self.target.timeout_secs.clamp(1, 60)),
                    client,
                };
                handle.spawn(worker.run(receiver));
                Some(sender)
            })
            .as_ref()
    }
}

/// The body of one delivery.
#[must_use]
pub fn body(event: &str, profile: &str, payload: &Payload, call: &CallInfo, id: &str) -> Value {
    let mut extra = payload.fields().clone();
    let tool_name = extra.remove("tool").unwrap_or(Value::Null);
    let tool_input = extra.remove("arguments").unwrap_or(Value::Null);
    extra.remove("schema_version");
    let session = extra
        .remove("session_id")
        .or_else(|| call.session_id.clone().map(Value::from))
        .unwrap_or(Value::Null);
    json!({
        "hook_event_name": event,
        "profile": profile,
        "tool_name": tool_name,
        "tool_input": tool_input,
        "session_id": session,
        "cwd": call.cwd,
        "extra": extra,
        "delivery_id": id,
        "timestamp": jiff::Timestamp::now().to_string(),
    })
}

/// HMAC-SHA256 (RFC 2104) of `message` under `key`.
fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    use sha2::{Digest as _, Sha256};
    const BLOCK: usize = 64;
    let mut block = [0u8; BLOCK];
    if key.len() > BLOCK {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let pad = |byte: u8| block.map(|b| b ^ byte);
    let inner = Sha256::new()
        .chain_update(pad(0x36))
        .chain_update(message)
        .finalize();
    Sha256::new()
        .chain_update(pad(0x5c))
        .chain_update(inner)
        .finalize()
        .into()
}

/// `sha256=<hex>` of the HMAC-SHA256 of `body` under `secret`.
#[must_use]
pub fn signature(secret: &str, body: &[u8]) -> String {
    let mac = hmac_sha256(secret.as_bytes(), body);
    let hex = mac.iter().fold(String::new(), |mut hex, b| {
        use std::fmt::Write as _;
        let _ = write!(hex, "{b:02x}");
        hex
    });
    format!("sha256={hex}")
}

fn client(scope: &ProfileScope, target: &OutboundHook) -> Result<reqwest::Client, String> {
    let config = scope.config();
    let host = reqwest::Url::parse(&target.url)
        .map_err(|e| e.to_string())?
        .host_str()
        .unwrap_or_default()
        .trim_matches(['[', ']'])
        .to_owned();
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    if !loopback {
        let egress = &config.security.egress;
        pmt_net::destination::DestinationPolicy::new(egress.allow_private, &egress.fake_ip_ranges)
            .map_err(|e| e.message().to_owned())?
            .check_host(&host)
            .map_err(|refusal| format!("egress policy refuses {host}: {refusal:?}"))?;
    }
    let policy = pmt_net::OutboundPolicy {
        force_ipv4: config.network.force_ipv4,
        extra_ca_file: config.network.extra_ca_file.as_ref().map(Into::into),
        proxy: config.network.proxy.clone(),
    };
    pmt_net::http::client_builder(&policy)
        .map_err(|e| e.message().to_owned())?
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| e.to_string())
}

struct Worker {
    name: String,
    url: String,
    secret: Option<String>,
    timeout: Duration,
    client: reqwest::Client,
}

impl Worker {
    async fn run(self, mut receiver: mpsc::Receiver<Delivery>) {
        while let Some(delivery) = receiver.recv().await {
            if self.send(&delivery).await.is_err() {
                tokio::time::sleep(RETRY_AFTER).await;
                if let Err(why) = self.send(&delivery).await {
                    tracing::warn!(webhook = %self.name, delivery = %delivery.id, "webhook delivery failed: {why}");
                }
            }
            PENDING.fetch_sub(1, Ordering::AcqRel);
        }
    }

    /// One attempt: `Err` when it is worth a retry (connection error, 5xx).
    async fn send(&self, delivery: &Delivery) -> Result<(), String> {
        let mut request = self
            .client
            .post(&self.url)
            .timeout(self.timeout)
            .header("content-type", "application/json")
            .header("X-Prometheus-Event", &delivery.event)
            .header("X-Prometheus-Delivery", &delivery.id)
            .body(delivery.body.clone());
        if let Some(secret) = &self.secret {
            request = request.header(
                "X-Prometheus-Signature-256",
                signature(secret, &delivery.body),
            );
        }
        match request.send().await {
            Ok(response) if response.status().is_server_error() => {
                Err(format!("HTTP {}", response.status()))
            }
            Ok(response) => {
                if !response.status().is_success() {
                    tracing::warn!(webhook = %self.name, delivery = %delivery.id, status = %response.status(), "webhook delivery refused");
                }
                Ok(())
            }
            Err(err) => Err(err.to_string()),
        }
    }
}

impl ExternalHook for Outbound {
    fn label(&self) -> &str {
        &self.label
    }

    fn applies(&self, hook: &str, payload: &Payload, _call: &CallInfo) -> bool {
        if !self.target.events.iter().any(|e| e == hook) {
            return false;
        }
        match (&self.matcher, TOOL_EVENTS.contains(&hook)) {
            (Some(matcher), true) => payload
                .fields()
                .get("tool")
                .and_then(Value::as_str)
                .is_some_and(|tool| matcher.is_match(tool)),
            _ => true,
        }
    }

    fn call<'a>(
        &'a self,
        hook: &'a str,
        payload: &'a Payload,
        call: &'a CallInfo,
    ) -> HookFuture<'a> {
        let id = format!("dlv_{}", ulid::Ulid::generate());
        let body = body(hook, &self.profile, payload, call, &id)
            .to_string()
            .into_bytes();
        let delivery = Delivery {
            event: hook.to_owned(),
            id,
            body,
        };
        if let Some(queue) = self.queue() {
            PENDING.fetch_add(1, Ordering::AcqRel);
            if queue.try_send(delivery).is_err() {
                PENDING.fetch_sub(1, Ordering::AcqRel);
                tracing::warn!(webhook = %self.target.name, "webhook queue full: delivery dropped");
            }
        } else {
            tracing::warn!(webhook = %self.target.name, "webhook not delivered: no client");
        }
        Box::pin(async { Ok(None) })
    }
}

/// Every configured target of `scope` as an external callback, with the
/// events it takes.
#[must_use]
pub fn targets(scope: &ProfileScope) -> Vec<(Vec<String>, Arc<dyn ExternalHook>)> {
    scope
        .config()
        .hooks
        .outbound
        .iter()
        .map(|target| {
            let events = target.events.clone();
            let hook: Arc<dyn ExternalHook> = Arc::new(Outbound::new(scope, target.clone()));
            (events, hook)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_signature_is_the_hmac_of_the_raw_body() {
        // RFC 4231 test case 2.
        assert_eq!(
            signature("Jefe", b"what do ya want for nothing?"),
            "sha256=5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }
}

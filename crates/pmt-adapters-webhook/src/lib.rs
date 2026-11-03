//! The first-party webhook adapter (spec: platform-adapters, Webhook
//! routes; Shared HTTP listener for webhook-style platforms): each route
//! `[gateway.platforms.webhook.routes.<name>]` is served at
//! `/p/<profile>/webhook/<name>` on the daemon's shared listener. A request
//! must carry a valid signature under the route's secret (unless the
//! route admits unsigned requests, which only a loopback listener allows);
//! its event type must be one the route accepts; its JSON payload becomes
//! the turn's input through the route's prompt template. Each delivery is
//! its own conversation thread of the route; replies are recorded under
//! `<profile>/logs/webhook/<route>.jsonl` (`log` delivery). Because only
//! verified requests reach the adapter, it vouches for its senders
//! (`enforces_own_access_policy = allowlist`).
//!
//! Before a turn starts, a request passes, in order: the route's
//! per-minute limit, its event list, its declarative filters, the
//! one-hour idempotency window and its transform script. A
//! `deliver_only` route then forwards the rendered prompt to its target
//! without any model call.

mod filter;
mod guard;
mod render;
mod script;
mod signature;

/// The mock platform the conformance kit runs this adapter against; only
/// in builds with the `kit` feature (tests, `prometheus` with
/// `adapter-kits`), so release binaries do not carry it.
#[cfg(feature = "kit")]
pub mod mock;

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pmt_config::ProfileScope;
use pmt_config::schema::WebhookRoute;
use pmt_gateway::messaging::adapter::credential;
use pmt_gateway::messaging::sign::hex;
use pmt_gateway::messaging::{
    AccessPolicy, Adapter, AdapterError, AdapterFactory, AdapterFuture, AdapterLink, AdapterSetup,
    Admission, ChatInfo, ChatType, Descriptor, HttpRequest, HttpResponse, Inbound, Interactive,
    LengthUnit, Outgoing, SendError, SenderVerdict, Sent, SessionHints,
};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
pub use signature::github_signature;

/// The platform name.
pub const PLATFORM: &str = "webhook";

/// The factory the daemon registers for `webhook`.
#[must_use]
pub fn factory() -> AdapterFactory {
    Arc::new(|setup| Ok(Arc::new(Webhook::new(setup)) as Arc<dyn Adapter>))
}

/// The sender id of a route's deliveries.
fn sender(route: &str) -> String {
    format!("webhook:{route}")
}

/// The webhook routes of one profile.
pub struct Webhook {
    scope: ProfileScope,
    routes: BTreeMap<String, WebhookRoute>,
    /// Each route's secret (`None`: unsigned requests admitted); unset
    /// until connected.
    secrets: Mutex<Option<BTreeMap<String, Option<String>>>>,
    link: Mutex<Option<Arc<dyn AdapterLink>>>,
    sent: AtomicU64,
    max_body: usize,
    guard: guard::Guard,
}

impl std::fmt::Debug for Webhook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Webhook")
            .field("profile", &self.scope.name().as_str())
            .field("routes", &self.routes.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

fn status(code: u16, body: &Value) -> HttpResponse {
    HttpResponse::json(code, body)
}

/// One verified request on its way to a turn or a target.
struct Call<'a> {
    route_name: &'a str,
    route: &'a WebhookRoute,
    event: Option<&'a str>,
    delivery: &'a str,
    text: &'a str,
}

/// The event type GitHub, GitLab or the payload names.
fn event_type(request: &HttpRequest, payload: &Value) -> Option<String> {
    request
        .header("x-github-event")
        .or_else(|| request.header("x-gitlab-event"))
        .map(str::to_owned)
        .or_else(|| payload["event_type"].as_str().map(str::to_owned))
}

/// The delivery's id: the sender's, or the body's digest.
fn delivery_id(request: &HttpRequest) -> String {
    ["x-github-delivery", "svix-id", "webhook-id", "x-request-id"]
        .iter()
        .find_map(|name| request.header(name))
        .map_or_else(
            || hex(&Sha256::digest(&request.body))[..32].to_owned(),
            str::to_owned,
        )
}

impl Webhook {
    /// The adapter for `setup`.
    #[must_use]
    pub fn new(setup: &AdapterSetup) -> Self {
        Self {
            scope: setup.scope.clone(),
            routes: setup.config.routes.clone(),
            secrets: Mutex::default(),
            link: Mutex::default(),
            sent: AtomicU64::new(0),
            max_body: usize::try_from(setup.config.webhook_body_limit()).unwrap_or(usize::MAX),
            guard: guard::Guard::default(),
        }
    }

    fn log_file(&self, route: &str) -> PathBuf {
        self.scope
            .path("logs")
            .join(PLATFORM)
            .join(format!("{route}.jsonl"))
    }

    /// Unsigned routes need a listener nobody else can reach.
    fn check_unsigned(&self) -> Result<(), AdapterError> {
        let Some(route) = self
            .routes
            .iter()
            .find(|(_, r)| r.insecure_no_auth)
            .map(|(n, _)| n)
        else {
            return Ok(());
        };
        let daemon = pmt_config::daemon::DaemonConfig::load(self.scope.home()).map_err(|e| {
            AdapterError {
                retryable: false,
                code: "config_invalid".to_owned(),
                message: e.message().to_owned(),
            }
        })?;
        let loopback = daemon
            .gateway
            .http
            .listen
            .parse::<std::net::SocketAddr>()
            .is_ok_and(|a| pmt_net::listen::is_loopback(a.ip()));
        if loopback {
            Ok(())
        } else {
            Err(AdapterError {
                retryable: false,
                code: "webhook_unsigned_public".to_owned(),
                message: format!(
                    "gateway.platforms.webhook.routes.{route}.insecure_no_auth is allowed only while gateway.http.listen is a loopback address"
                ),
            })
        }
    }

    fn connect_with(&self, link: Arc<dyn AdapterLink>) -> Result<(), AdapterError> {
        self.check_unsigned()?;
        let mut secrets = BTreeMap::new();
        for (name, route) in &self.routes {
            let secret = match (&route.secret_secret, route.insecure_no_auth) {
                (Some(secret), _) => Some(credential(&self.scope, &secret.0)?),
                (None, true) => None,
                (None, false) => {
                    return Err(AdapterError {
                        retryable: false,
                        code: "missing_option".to_owned(),
                        message: format!(
                            "gateway.platforms.webhook.routes.{name}.secret_secret is required unless insecure_no_auth = true"
                        ),
                    });
                }
            };
            secrets.insert(name.clone(), secret);
        }
        *self.secrets.lock().expect("secrets poisoned") = Some(secrets);
        *self.link.lock().expect("link poisoned") = Some(link);
        Ok(())
    }

    /// Handles a verified request for `route`.
    async fn deliver(&self, route_name: &str, request: &HttpRequest) -> HttpResponse {
        let Some(route) = self.routes.get(route_name) else {
            return status(404, &json!({"error": "unknown route"}));
        };
        if request.method != "POST" {
            return status(405, &json!({"error": "POST only"}));
        }
        let now = tokio::time::Instant::now();
        if !self.guard.admit(route_name, route.rate_limit_per_min, now) {
            return status(429, &json!({"status": "rate_limited"}));
        }
        let Ok(payload) = serde_json::from_slice::<Value>(&request.body) else {
            return status(400, &json!({"error": "the body is not JSON"}));
        };
        let event = event_type(request, &payload);
        if !route.events.is_empty() && !event.as_ref().is_some_and(|e| route.events.contains(e)) {
            return status(200, &json!({"status": "ignored", "event": event}));
        }
        if !self.passes_filters(route, &payload, event.as_deref(), request) {
            return status(
                200,
                &json!({"status": "ignored", "reason": "filter", "route": route_name}),
            );
        }
        let delivery = delivery_id(request);
        if self.guard.seen(route_name, &delivery, now) {
            return status(200, &json!({"status": "duplicate"}));
        }
        let Some(payload) = self.transform(route_name, route, payload).await else {
            self.guard.remember(route_name, &delivery, now);
            return status(200, &json!({"status": "ignored", "reason": "script"}));
        };
        let text = render::render(route.prompt.as_deref(), &payload);
        let call = Call {
            route_name,
            route,
            event: event.as_deref(),
            delivery: &delivery,
            text: &text,
        };
        let (answer, handled) = if route.deliver_only {
            self.forward(&call).await
        } else {
            self.start_turn(&call).await
        };
        if handled {
            self.guard.remember(route_name, &delivery, now);
        }
        answer
    }

    fn passes_filters(
        &self,
        route: &WebhookRoute,
        payload: &Value,
        event: Option<&str>,
        request: &HttpRequest,
    ) -> bool {
        let Some(filters) = &route.filters else {
            return true;
        };
        let subject = filter::Subject {
            payload,
            event,
            request,
            profile_dir: self.scope.dir(),
        };
        filter::matches(filters.conditions(), &subject)
    }

    /// The payload after the route's transform script; `None` drops the
    /// event.
    async fn transform(&self, name: &str, route: &WebhookRoute, payload: Value) -> Option<Value> {
        let Some(script) = &route.script else {
            return Some(payload);
        };
        let timeout = Duration::from_secs(route.script_timeout_secs.max(1));
        match script::run(&self.scope, name, script, timeout, &payload).await {
            script::Outcome::Payload(next) => Some(next),
            script::Outcome::Ignore(why) => {
                tracing::info!(platform = PLATFORM, route = name, "event ignored: {why}");
                None
            }
        }
    }

    /// Sends the rendered text to the route's target, without a turn.
    async fn forward(&self, call: &Call<'_>) -> (HttpResponse, bool) {
        let Some(link) = self.link.lock().expect("link poisoned").clone() else {
            return (HttpResponse::status(503), false);
        };
        let target = call.route.deliver_target();
        match link.deliver(target, call.text).await {
            Ok(destination) => {
                tracing::info!(
                    platform = PLATFORM,
                    route = call.route_name,
                    destination,
                    "webhook event forwarded"
                );
                let body = json!({"status": "delivered", "route": call.route_name,
                                  "target": target, "delivery_id": call.delivery});
                (status(200, &body), true)
            }
            Err(why) => {
                tracing::warn!(
                    platform = PLATFORM,
                    route = call.route_name,
                    "webhook delivery to {target} failed: {why}"
                );
                (status(502, &json!({"error": "Delivery failed"})), false)
            }
        }
    }

    /// Submits the rendered text as a turn of the delivery's own thread.
    async fn start_turn(&self, call: &Call<'_>) -> (HttpResponse, bool) {
        let Some(link) = self.link.lock().expect("link poisoned").clone() else {
            return (HttpResponse::status(503), false);
        };
        let mut inbound = Inbound::text(
            ChatType::Dm,
            call.route_name,
            &sender(call.route_name),
            call.text,
        );
        inbound.message_id = call.delivery.to_owned();
        inbound.sender_name = Some(format!("webhook {}", call.route_name));
        // Each delivery runs in its own conversation.
        inbound.thread_id = Some(call.delivery.to_owned());
        let body = json!({"status": "accepted", "route": call.route_name, "event": call.event,
                          "delivery_id": call.delivery});
        match link.submit(inbound).await {
            Admission::Accepted | Admission::Queued => (status(202, &body), true),
            Admission::Duplicate => (status(200, &json!({"status": "duplicate"})), true),
            Admission::RateLimited => (status(429, &json!({"status": "rate_limited"})), false),
            Admission::Unauthorized | Admission::PairingIssued => {
                (status(403, &json!({"status": "unauthorized"})), false)
            }
            Admission::Dropped => (status(503, &json!({"status": "dropped"})), false),
            Admission::Paused => (status(503, &json!({"status": "paused"})), false),
            Admission::Handled => (status(200, &json!({"status": "handled"})), true),
        }
    }

    fn record(&self, message: &Outgoing) -> Result<Sent, SendError> {
        if !self.routes.contains_key(&message.chat_id) {
            return Err(SendError::Permanent(format!(
                "no webhook route named '{}'",
                message.chat_id
            )));
        }
        let n = self.sent.fetch_add(1, Ordering::Relaxed);
        let id = format!(
            "{}:{}-{n}",
            message.chat_id,
            jiff::Timestamp::now().as_millisecond()
        );
        let line = json!({
            "at": jiff::Timestamp::now().to_string(),
            "route": message.chat_id,
            "delivery_id": message.thread_id,
            "message_id": id,
            "text": message.text,
        });
        let file = self.log_file(&message.chat_id);
        let written = file
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| {
                let mut out = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&file)?;
                writeln!(out, "{line}")
            });
        written.map_err(|e| SendError::Permanent(format!("{}: {e}", file.display())))?;
        tracing::info!(
            platform = PLATFORM,
            route = message.chat_id,
            "webhook reply recorded"
        );
        Ok(Sent {
            message_id: Some(id),
        })
    }
}

impl Adapter for Webhook {
    fn describe(&self) -> Descriptor {
        let limit = pmt_config::platforms::first_party(PLATFORM)
            .map_or(65_536, |p| p.max_message_length as usize);
        Descriptor {
            length_unit: LengthUnit::Utf8Bytes,
            interactive: Interactive::None,
            binds_http: true,
            access_policy: Some(AccessPolicy::Allowlist),
            ..Descriptor::new(PLATFORM, limit)
        }
    }

    fn connect(&self, link: Arc<dyn AdapterLink>) -> AdapterFuture<'_, Result<(), AdapterError>> {
        let connected = self.connect_with(link);
        Box::pin(async move { connected })
    }

    fn disconnect(&self) -> AdapterFuture<'_, ()> {
        self.link.lock().expect("link poisoned").take();
        Box::pin(async {})
    }

    fn send(&self, message: &Outgoing) -> AdapterFuture<'_, Result<Sent, SendError>> {
        let sent = self.record(message);
        Box::pin(async move { sent })
    }

    fn chat_info<'a>(&'a self, chat_id: &'a str) -> AdapterFuture<'a, Result<ChatInfo, SendError>> {
        Box::pin(async move {
            if self.routes.contains_key(chat_id) {
                Ok(ChatInfo {
                    name: format!("webhook route {chat_id}"),
                    chat_type: ChatType::Dm,
                })
            } else {
                Err(SendError::Permanent(format!(
                    "no webhook route named '{chat_id}'"
                )))
            }
        })
    }

    fn authorize_sender<'a>(
        &'a self,
        inbound: &'a Inbound,
    ) -> AdapterFuture<'a, Result<SenderVerdict, AdapterError>> {
        // Only verified requests become events: a route's own sender is
        // vouched for, anything else is not this adapter's to judge.
        let ours = self.routes.contains_key(&inbound.chat_id)
            && inbound.sender_id == sender(&inbound.chat_id);
        Box::pin(async move {
            Ok(if ours {
                SenderVerdict::Allow
            } else {
                SenderVerdict::Unknown
            })
        })
    }

    fn verify_http(&self, request: &HttpRequest) -> bool {
        let route = request
            .path
            .trim_start_matches('/')
            .split('/')
            .next()
            .unwrap_or_default();
        let secrets = self.secrets.lock().expect("secrets poisoned");
        let Some(secrets) = secrets.as_ref() else {
            return false;
        };
        match secrets.get(route) {
            // Unknown routes are answered 404 and admit nothing; a route
            // without a secret admits unsigned requests.
            None | Some(None) => true,
            Some(Some(secret)) => {
                signature::verify(request, secret, jiff::Timestamp::now().as_second())
            }
        }
    }

    fn max_body_bytes(&self) -> usize {
        self.max_body
    }

    fn session_hints(&self, chat_id: &str, _thread_id: Option<&str>) -> SessionHints {
        self.routes
            .get(chat_id)
            .filter(|r| !r.deliver_only)
            .map(|route| SessionHints {
                skills: route.skills.clone(),
                toolsets: route.toolsets.clone(),
            })
            .unwrap_or_default()
    }

    fn handle_http<'a>(&'a self, request: &'a HttpRequest) -> AdapterFuture<'a, HttpResponse> {
        Box::pin(async move {
            let route = request
                .path
                .trim_start_matches('/')
                .split('/')
                .next()
                .unwrap_or_default();
            self.deliver(route, request).await
        })
    }
}

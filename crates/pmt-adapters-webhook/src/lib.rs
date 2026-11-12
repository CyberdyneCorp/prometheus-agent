//! The first-party webhook adapter (spec: platform-adapters, Webhook
//! routes; Shared HTTP listener for webhook-style platforms): each route
//! `[gateway.platforms.webhook.routes.<name>]` is served at
//! `/p/<profile>/webhook/<name>` on the daemon's shared listener. A request
//! must carry a valid signature under the route's secret (unless the
//! route admits unsigned requests, which only a loopback listener allows);
//! its event type must be one the route accepts; its JSON payload becomes
//! the turn's input through the route's prompt template. Each delivery is
//! its own conversation thread of the route, run with the read-only
//! `webhook-safe` toolset unless the route names toolsets. The answer goes
//! where the route's `deliver` says (`delivery`): back to the HTTP caller
//! (`origin`, the default), the route's log under
//! `<profile>/logs/webhook/<route>.jsonl`, a GitHub comment or a send
//! target. Because only verified requests reach the adapter, it vouches
//! for its senders (`enforces_own_access_policy = allowlist`).
//!
//! Before a turn starts, a request passes, in order: the route's
//! per-minute limit, its event list, its declarative filters, the
//! one-hour idempotency window and its transform script. A
//! `deliver_only` route then forwards the rendered prompt to its target
//! without any model call; a `cron_job` route fires that scheduled job
//! with the rendered prompt as its run context (`jobs`).

mod delivery;
mod filter;
mod guard;
mod jobs;
mod render;
mod routes;
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
use pmt_config::platforms::WebhookOptions;
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
    routes: routes::Routes,
    /// Each route's secret (`None`: unsigned requests admitted); unset
    /// until connected.
    secrets: Mutex<Option<BTreeMap<String, Option<String>>>>,
    link: Mutex<Option<Arc<dyn AdapterLink>>>,
    sent: AtomicU64,
    max_body: usize,
    guard: guard::Guard,
    options: WebhookOptions,
    /// For `github_comment` (absent when it cannot be built).
    client: Option<reqwest::Client>,
    deliveries: delivery::Deliveries,
}

/// The toolset of a route's runs unless it names its own.
const DEFAULT_TOOLSET: &str = "webhook-safe";

impl std::fmt::Debug for Webhook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Webhook")
            .field("profile", &self.scope.name().as_str())
            .field(
                "routes",
                &self.routes.configured().keys().collect::<Vec<_>>(),
            )
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
    payload: &'a Value,
}

/// The event type GitHub, GitLab or the payload names.
fn event_type(request: &HttpRequest, payload: &Value) -> Option<String> {
    request
        .header("x-github-event")
        .or_else(|| request.header("x-gitlab-event"))
        .or_else(|| request.header(INTERNAL_EVENT))
        .map(str::to_owned)
        .or_else(|| payload["event_type"].as_str().map(str::to_owned))
}

/// The header carrying an internal event's type (MCP notifications the
/// gateway admits on a route, `Gateway::mcp_event`).
const INTERNAL_EVENT: &str = pmt_gateway::messaging::mcp_events::EVENT_HEADER;

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
        let options: WebhookOptions =
            pmt_config::platforms::options(PLATFORM, &setup.config.options).unwrap_or_default();
        let client = pmt_gateway::messaging::proxy::client_builder(setup)
            .ok()
            .and_then(|(builder, _)| builder.build().ok());
        Self {
            scope: setup.scope.clone(),
            routes: routes::Routes::new(setup.config.routes.clone()),
            secrets: Mutex::default(),
            link: Mutex::default(),
            sent: AtomicU64::new(0),
            max_body: usize::try_from(setup.config.webhook_body_limit()).unwrap_or(usize::MAX),
            guard: guard::Guard::default(),
            options,
            client,
            deliveries: delivery::Deliveries::default(),
        }
    }

    /// Every route served now (configured and subscribed).
    fn served(&self) -> Arc<routes::RouteMap> {
        self.routes.current(&self.scope)
    }

    fn log_file(&self, route: &str) -> PathBuf {
        self.scope
            .path("logs")
            .join(PLATFORM)
            .join(format!("{route}.jsonl"))
    }

    /// Unsigned routes need a listener nobody else can reach: the address
    /// the daemon actually bound, not `daemon.toml` as it reads now.
    fn check_unsigned(
        link: &dyn AdapterLink,
        routes: &BTreeMap<String, WebhookRoute>,
    ) -> Result<(), AdapterError> {
        let Some(route) = routes
            .iter()
            .find(|(_, r)| r.insecure_no_auth)
            .map(|(n, _)| n)
        else {
            return Ok(());
        };
        let loopback = link
            .http_listen()
            .is_some_and(|a| pmt_net::listen::is_loopback(a.ip()));
        if loopback {
            Ok(())
        } else {
            Err(AdapterError {
                retryable: false,
                code: "webhook_unsigned_public".to_owned(),
                message: format!(
                    "gateway.platforms.webhook.routes.{route}.insecure_no_auth is allowed only while the gateway HTTP listener is bound to a loopback address (restart the daemon after changing gateway.http.listen)"
                ),
            })
        }
    }

    fn connect_with(&self, link: Arc<dyn AdapterLink>) -> Result<(), AdapterError> {
        Self::check_unsigned(link.as_ref(), self.routes.configured())?;
        let mut secrets = BTreeMap::new();
        for (name, route) in self.routes.configured() {
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
        let routes = self.served();
        let Some(route) = routes.get(route_name) else {
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
            payload: &payload,
        };
        let (answer, handled) = if let Some(job) = &route.cron_job {
            self.fire_job(&call, job).await
        } else if route.deliver_only {
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
        let target = call.route.deliver_target();
        let github = (target == "github_comment")
            .then(|| delivery::GithubThread::of(call.payload))
            .flatten();
        let answer = self
            .deliveries
            .open(call.delivery, target == "origin", github);
        match link.submit(inbound).await {
            Admission::Accepted | Admission::Queued => match answer {
                Some(answer) => (self.wait_for(call, answer, body).await, true),
                None => (status(202, &body), true),
            },
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

    /// `origin`: the run's answer as the HTTP response, unless it takes
    /// longer than the route waits (then 202 and the answer is logged).
    async fn wait_for(
        &self,
        call: &Call<'_>,
        answer: tokio::sync::oneshot::Receiver<String>,
        accepted: Value,
    ) -> HttpResponse {
        let wait = Duration::from_secs(call.route.origin_wait_secs.max(1));
        if let Ok(Ok(text)) = tokio::time::timeout(wait, answer).await {
            return status(
                200,
                &json!({"status": "completed", "route": call.route_name, "event": call.event,
                        "delivery_id": call.delivery, "answer": text}),
            );
        }
        self.deliveries.give_up(call.delivery);
        status(202, &accepted)
    }

    /// Delivers one answer of a run as its route says, then to each
    /// `deliver_extra` target.
    async fn answer(&self, message: &Outgoing) -> Result<Sent, SendError> {
        let routes = self.served();
        let Some(route) = routes.get(&message.chat_id) else {
            return self.record(message);
        };
        let delivery = message.thread_id.as_deref().unwrap_or_default();
        let sent = match route.deliver_target() {
            "log" => self.record(message)?,
            "origin" => {
                let answered = self
                    .deliveries
                    .waiting(delivery)
                    .is_some_and(|caller| caller.send(message.text.clone()).is_ok());
                if answered {
                    Sent {
                        message_id: Some(format!("{}:{delivery}", message.chat_id)),
                    }
                } else {
                    // The caller gave up waiting: the answer is kept in the log.
                    self.record(message)?
                }
            }
            "github_comment" => self.github_comment(route, delivery, &message.text).await?,
            target => {
                self.forward_answer(route, &message.chat_id, target, &message.text)
                    .await?
            }
        };
        for target in &route.deliver_extra {
            if let Err(err) = self
                .forward_answer(route, &message.chat_id, target, &message.text)
                .await
            {
                tracing::warn!(
                    platform = PLATFORM,
                    route = message.chat_id,
                    "deliver_extra {target}: {}",
                    err.describe()
                );
            }
        }
        Ok(sent)
    }

    /// An answer to a send target (mirrored when the route asks).
    async fn forward_answer(
        &self,
        route: &WebhookRoute,
        route_name: &str,
        target: &str,
        text: &str,
    ) -> Result<Sent, SendError> {
        let link = self
            .link
            .lock()
            .expect("link poisoned")
            .clone()
            .ok_or_else(|| SendError::Network("the webhook adapter is not connected".to_owned()))?;
        let label = link
            .deliver(target, text)
            .await
            .map_err(SendError::Permanent)?;
        if route.mirror_to_session {
            link.mirror(target, &format!("[Webhook delivery: {route_name}]\n{text}"))
                .await;
        }
        Ok(Sent {
            message_id: Some(label),
        })
    }

    /// An answer as a comment on the payload's pull request or issue.
    async fn github_comment(
        &self,
        route: &WebhookRoute,
        delivery: &str,
        text: &str,
    ) -> Result<Sent, SendError> {
        let thread = self.deliveries.github(delivery).ok_or_else(|| {
            SendError::Permanent("the payload names no pull request or issue".to_owned())
        })?;
        let secret = route
            .github_token_secret
            .as_ref()
            .ok_or_else(|| SendError::Permanent("no github_token_secret".to_owned()))?;
        let api = &self.options.github_api_url;
        if !pmt_config::daemon_relay::secure_url(api, "https", "http") {
            return Err(SendError::Permanent(format!(
                "gateway.platforms.webhook.options.github_api_url: '{api}' would send the GitHub token in cleartext; use https://"
            )));
        }
        let token =
            credential(&self.scope, &secret.0).map_err(|e| SendError::Permanent(e.message))?;
        let client = self
            .client
            .as_ref()
            .ok_or_else(|| SendError::Permanent("no HTTP client".to_owned()))?;
        delivery::comment(client, api, &token, &thread, text).await?;
        Ok(Sent {
            message_id: Some(format!("{}#{}", thread.repo, thread.number)),
        })
    }

    fn record(&self, message: &Outgoing) -> Result<Sent, SendError> {
        if !self.served().contains_key(&message.chat_id) {
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
    fn reach(&self) -> Result<pmt_gateway::messaging::probe::Reach, String> {
        // Deliveries arrive on the daemon's listener; replies go back to
        // per-route URLs only when an event asks.
        Ok(pmt_gateway::messaging::probe::Reach::InboundOnly)
    }

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
        Box::pin(async move {
            jobs::check_routes(link.as_ref(), self.routes.configured()).await?;
            self.connect_with(link)
        })
    }

    fn disconnect(&self) -> AdapterFuture<'_, ()> {
        self.link.lock().expect("link poisoned").take();
        Box::pin(async {})
    }

    fn send(&self, message: &Outgoing) -> AdapterFuture<'_, Result<Sent, SendError>> {
        let message = message.clone();
        Box::pin(async move { self.answer(&message).await })
    }

    fn chat_info<'a>(&'a self, chat_id: &'a str) -> AdapterFuture<'a, Result<ChatInfo, SendError>> {
        Box::pin(async move {
            if self.served().contains_key(chat_id) {
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
        let ours = self.served().contains_key(&inbound.chat_id)
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
        let served = self.served();
        let secrets = self.secrets.lock().expect("secrets poisoned");
        let Some(secrets) = secrets.as_ref() else {
            return false;
        };
        // A subscribed route is signed with its own generated secret.
        let subscribed = (served.contains_key(route) && !secrets.contains_key(route))
            .then(|| self.routes.subscribed_secret(route));
        if let Some(secret) = subscribed {
            return secret.is_some_and(|secret| {
                signature::verify(request, &secret, jiff::Timestamp::now().as_second())
            });
        }
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
        self.served()
            .get(chat_id)
            .filter(|r| !r.deliver_only)
            .map(|route| SessionHints {
                skills: route.skills.clone(),
                // Read-only unless the route names toolsets.
                toolsets: Some(
                    route
                        .toolsets
                        .clone()
                        .unwrap_or_else(|| vec![DEFAULT_TOOLSET.to_owned()]),
                ),
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

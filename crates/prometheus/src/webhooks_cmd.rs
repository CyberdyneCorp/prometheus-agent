//! `prometheus webhooks subscribe|list|remove|test` (spec: cli, Webhook
//! subscriptions): webhook routes kept in the profile's state
//! (`pmt_config::webhook_subscriptions`, never `config.toml`), served by
//! the webhook adapter at `/p/<profile>/webhook/<name>` on the daemon's
//! shared listener next to the configured routes. A subscription without
//! `--secret-name` gets a new 32-byte signing secret in the secrets store,
//! printed once; `test` signs a sample payload with it and posts it
//! through the same listener.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;

use clap::{Args, Subcommand};
use pmt_config::ProfileScope;
use pmt_config::schema::{SecretName, WebhookRoute};
use pmt_config::webhook_subscriptions::{self, Subscription};
use pmt_core::error::{Error, codes};
use serde_json::json;

use crate::context::Context;

/// `prometheus webhooks` subcommands.
#[derive(Debug, Subcommand)]
pub enum WebhooksCommand {
    /// Create a webhook route; prints its URL and (once) its new signing
    /// secret.
    Subscribe(SubscribeArgs),
    /// List the subscribed routes (never their secrets).
    #[command(visible_alias = "ls")]
    List,
    /// Remove a subscribed route (and the signing secret made for it).
    #[command(visible_alias = "rm")]
    Remove {
        /// The route.
        name: String,
    },
    /// Post a signed sample payload to a route through the daemon's
    /// listener; prints the session it started.
    Test {
        /// The route.
        name: String,
        /// The JSON payload to send instead of the sample.
        #[arg(long, value_name = "FILE")]
        payload: Option<PathBuf>,
    },
}

/// `webhooks subscribe` flags: one per route field.
#[derive(Debug, Clone, Args)]
#[allow(clippy::struct_excessive_bools)] // independent switches of one route
pub struct SubscribeArgs {
    /// The route name (a-z, 0-9, '-', '_').
    pub name: String,
    /// Event types admitted (comma-separated; default: every event).
    #[arg(long, value_name = "LIST", value_delimiter = ',')]
    pub events: Vec<String>,
    /// Skills preloaded into each delivery's session (comma-separated).
    #[arg(long, value_name = "LIST", value_delimiter = ',')]
    pub skills: Vec<String>,
    /// Where the answer goes: origin, log, `github_comment` or a send
    /// target `platform[:chat_id[:thread_id]]` (default: origin).
    #[arg(long, value_name = "TARGET")]
    pub deliver: Option<String>,
    /// The turn's input, `{dot.path}` placeholders taken from the payload.
    #[arg(long, value_name = "TEMPLATE")]
    pub prompt: Option<String>,
    /// Sign with this existing secret instead of a generated one.
    #[arg(long, value_name = "NAME")]
    pub secret_name: Option<String>,
    /// Send the rendered prompt to `--deliver` with no model call.
    #[arg(long)]
    pub deliver_only: bool,
    /// A transform script under the profile's `scripts/` directory.
    #[arg(long, value_name = "PATH")]
    pub script: Option<String>,
    /// Fire this scheduled job (id or name) instead of a turn.
    #[arg(long, value_name = "JOB")]
    pub cron_job: Option<String>,
    /// Also append the delivered text to the target chat's session.
    #[arg(long)]
    pub mirror_to_session: bool,
}

/// Runs `prometheus webhooks`.
///
/// # Errors
///
/// Configuration, secrets, store and network errors.
pub fn run(command: &WebhooksCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    match command {
        WebhooksCommand::Subscribe(args) => subscribe(&scope, args, ctx.json),
        WebhooksCommand::List => list(&scope, ctx.json),
        WebhooksCommand::Remove { name } => remove(&scope, name),
        WebhooksCommand::Test { name, payload } => test(&scope, name, payload.as_deref(), ctx.json),
    }
}

/// The route's URL on the daemon's shared listener.
fn url(scope: &ProfileScope, name: &str) -> String {
    let listen = pmt_config::DaemonConfig::load(scope.home())
        .map(|c| c.gateway.http.listen)
        .unwrap_or_default();
    format!("http://{listen}/p/{}/webhook/{name}", scope.name())
}

fn not_found(name: &str) -> Error {
    Error::new(
        &codes::WEBHOOK_NOT_FOUND,
        format!("no webhook subscription named '{name}'"),
    )
    .with_hint("run: prometheus webhooks list")
}

/// The route the flags describe, signed with `secret`.
fn route_of(args: &SubscribeArgs, secret: &str) -> WebhookRoute {
    WebhookRoute {
        events: args.events.clone(),
        secret_secret: Some(SecretName(secret.to_owned())),
        prompt: args.prompt.clone(),
        skills: args.skills.clone(),
        deliver: args.deliver.clone(),
        deliver_only: args.deliver_only,
        cron_job: args.cron_job.clone(),
        script: args.script.clone(),
        mirror_to_session: args.mirror_to_session,
        ..WebhookRoute::default()
    }
}

/// Refuses a name a configured route or another subscription has.
fn check_free(
    scope: &ProfileScope,
    subscriptions: &BTreeMap<String, Subscription>,
    name: &str,
) -> Result<(), Error> {
    let configured = scope
        .config()
        .gateway
        .platforms
        .get("webhook")
        .is_some_and(|p| p.routes.contains_key(name));
    if configured || subscriptions.contains_key(name) {
        return Err(Error::new(
            &codes::CONFIG_INVALID,
            format!("a webhook route named '{name}' already exists"),
        )
        .with_hint("pick another name, or remove it first: prometheus webhooks remove <name>"));
    }
    Ok(())
}

/// A subscription just created: its URL, and the generated secret (the
/// only time it is ever shown).
pub(crate) struct Created {
    pub url: String,
    pub secret_name: String,
    pub secret: Option<String>,
}

/// Creates the subscription `args` describe (with `description`).
///
/// # Errors
///
/// `config.invalid` for a taken name or a bad route, secrets and store
/// errors.
pub(crate) fn create(
    scope: &ProfileScope,
    args: &SubscribeArgs,
    description: &str,
) -> Result<Created, Error> {
    let name = args.name.as_str();
    let mut subscriptions = webhook_subscriptions::load(scope.dir())?;
    check_free(scope, &subscriptions, name)?;
    let secret_name = match &args.secret_name {
        Some(given) => {
            pmt_config::secrets::check_name(given)?;
            given.clone()
        }
        None => webhook_subscriptions::secret_name(name),
    };
    let mut route = route_of(args, &secret_name);
    description.clone_into(&mut route.description);
    let key = format!("webhooks.{name}");
    pmt_config::gateway_check::check_webhook_route(scope.config(), &key, name, &route)?;
    if let Some(job) = &route.cron_job {
        pmt_cron::ops::find(&pmt_store::Store::open(scope)?, job)?;
    }
    let generated = args
        .secret_name
        .is_none()
        .then(webhook_subscriptions::generate_secret);
    if let Some(secret) = &generated {
        scope.secrets().store().set(&secret_name, secret)?;
    } else if scope.secrets().resolve(&secret_name).is_err() {
        eprintln!(
            "warning: secret {secret_name} is not set; set it before events arrive: prometheus secrets set {secret_name}"
        );
    }
    subscriptions.insert(
        name.to_owned(),
        Subscription {
            created_at: jiff::Timestamp::now().to_string(),
            generated_secret: generated.is_some(),
            route,
        },
    );
    webhook_subscriptions::save(scope.dir(), &subscriptions)?;
    Ok(Created {
        url: url(scope, name),
        secret_name,
        secret: generated.as_ref().map(|s| s.expose().to_owned()),
    })
}

fn subscribe(scope: &ProfileScope, args: &SubscribeArgs, json: bool) -> Result<u8, Error> {
    let name = args.name.as_str();
    let Created {
        url,
        secret_name,
        secret,
    } = create(scope, args, "")?;
    if json {
        return crate::output::json(&json!({
            "name": name, "url": url, "secret_name": secret_name, "secret": secret,
        }));
    }
    let mut text = format!("subscribed webhook route '{name}'\nurl: {url}");
    match &secret {
        Some(secret) => {
            let _ = write!(
                text,
                "\nsecret: {secret}\n(shown once; stored as {secret_name}; sign requests with it)"
            );
        }
        None => {
            let _ = write!(text, "\nsigned with secret {secret_name}");
        }
    }
    if !webhook_enabled(scope) {
        text.push_str("\nnote: the webhook adapter is off; enable it: prometheus config set gateway.platforms.webhook.enabled true");
    }
    crate::output::print(&text)
}

/// Enables or disables subscription `name` (the adapter re-reads the
/// file, so no restart is needed).
///
/// # Errors
///
/// `webhook.not_found`, configuration errors.
pub(crate) fn set_enabled(scope: &ProfileScope, name: &str, enabled: bool) -> Result<(), Error> {
    let mut subscriptions = webhook_subscriptions::load(scope.dir())?;
    let found = subscriptions.get_mut(name).ok_or_else(|| not_found(name))?;
    found.route.enabled = enabled;
    webhook_subscriptions::save(scope.dir(), &subscriptions)
}

/// The subscriptions as the dashboard lists them (never a secret value),
/// and whether the webhook adapter is enabled.
///
/// # Errors
///
/// Configuration errors.
pub(crate) fn rows(scope: &ProfileScope) -> Result<serde_json::Value, Error> {
    let subscriptions = webhook_subscriptions::load(scope.dir())?;
    let rows: Vec<_> = subscriptions
        .iter()
        .map(|(name, s)| {
            json!({
                "name": name,
                "description": s.route.description,
                "url": url(scope, name),
                "events": s.route.events,
                "deliver": s.route.deliver_target(),
                "deliver_only": s.route.deliver_only,
                "enabled": s.route.enabled,
                "created_at": s.created_at,
            })
        })
        .collect();
    Ok(json!({"subscriptions": rows, "adapter_enabled": webhook_enabled(scope)}))
}

/// Removes subscription `name` (and the secret generated for it).
///
/// # Errors
///
/// `webhook.not_found`, configuration and secrets errors.
pub(crate) fn delete(scope: &ProfileScope, name: &str) -> Result<(), Error> {
    let mut subscriptions = webhook_subscriptions::load(scope.dir())?;
    let removed = subscriptions.remove(name).ok_or_else(|| not_found(name))?;
    webhook_subscriptions::save(scope.dir(), &subscriptions)?;
    if removed.generated_secret
        && let Some(secret) = &removed.route.secret_secret
    {
        scope.secrets().store().remove(&secret.0)?;
    }
    Ok(())
}

fn webhook_enabled(scope: &ProfileScope) -> bool {
    scope
        .config()
        .gateway
        .platforms
        .get("webhook")
        .is_some_and(|p| p.enabled)
}

fn list(scope: &ProfileScope, json: bool) -> Result<u8, Error> {
    let subscriptions = webhook_subscriptions::load(scope.dir())?;
    if json {
        let rows: Vec<_> = subscriptions
            .iter()
            .map(|(name, s)| {
                json!({
                    "name": name,
                    "url": url(scope, name),
                    "events": s.route.events,
                    "deliver": s.route.deliver_target(),
                    "deliver_only": s.route.deliver_only,
                    "cron_job": s.route.cron_job,
                    "secret_name": s.route.secret_secret.as_ref().map(|n| &n.0),
                    "created_at": s.created_at,
                })
            })
            .collect();
        return crate::output::json(&json!({"subscriptions": rows}));
    }
    if subscriptions.is_empty() {
        return crate::output::print(&"no webhook subscriptions");
    }
    let mut text = String::new();
    for (name, s) in &subscriptions {
        let events = if s.route.events.is_empty() {
            "*".to_owned()
        } else {
            s.route.events.join(",")
        };
        let secret = s.route.secret_secret.as_ref().map_or("-", |n| n.0.as_str());
        let _ = writeln!(
            text,
            "{name}  events {events}  deliver {}  secret {secret}  {}",
            s.route.deliver_target(),
            url(scope, name)
        );
    }
    crate::output::print(&text.trim_end())
}

fn remove(scope: &ProfileScope, name: &str) -> Result<u8, Error> {
    delete(scope, name)?;
    eprintln!("removed webhook route '{name}'");
    Ok(0)
}

/// A signed test delivery sent to route `name`: its delivery id and the
/// listener's status.
pub(crate) struct TestSent {
    pub delivery: String,
    pub status: u16,
    pub runs_turn: bool,
}

/// Sends a test event signed with route `name`'s secret to the daemon's
/// listener (`payload` replaces the default body).
///
/// # Errors
///
/// `webhook.not_found`, secrets errors, `webhook.test_failed` when the
/// listener is unreachable or does not accept it.
#[cfg(feature = "gateway")]
pub(crate) async fn send_test(
    scope: &ProfileScope,
    name: &str,
    payload: Option<Vec<u8>>,
) -> Result<TestSent, Error> {
    let subscriptions = webhook_subscriptions::load(scope.dir())?;
    let route = &subscriptions
        .get(name)
        .ok_or_else(|| not_found(name))?
        .route;
    let secret_name = route
        .secret_secret
        .as_ref()
        .ok_or_else(|| not_found(name))?;
    let secret = scope.secrets().resolve(&secret_name.0)?;
    let event = route.events.first().map_or("test", String::as_str);
    let body = payload.unwrap_or_else(|| {
        serde_json::to_vec(&json!({"event_type": event, "test": true, "route": name}))
            .unwrap_or_default()
    });
    let delivery = format!("test-{}", ulid::Ulid::generate().to_string().to_lowercase());
    let signature = pmt_adapters_webhook::github_signature(secret.expose(), &body);
    let url = url(scope, name);
    // The daemon's listener is on this machine: reached directly, never
    // through a configured proxy.
    let client =
        pmt_net::http::client_builder_loopback_direct(&pmt_net::http::OutboundPolicy::default())?
            .build()
            .map_err(|e| Error::new(&codes::WEBHOOK_TEST_FAILED, format!("no HTTP client: {e}")))?;
    let response = client
        .post(&url)
        .header("content-type", "application/json")
        .header("x-github-event", event)
        .header("x-github-delivery", &delivery)
        .header("x-hub-signature-256", signature)
        .body(body)
        .send()
        .await
        .map_err(|e| {
            Error::new(
                &codes::WEBHOOK_TEST_FAILED,
                format!("cannot reach the webhook listener at {url}: {e}"),
            )
            .with_hint("start the daemon with the webhook adapter enabled: prometheus daemon start")
        })?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(Error::new(
            &codes::WEBHOOK_TEST_FAILED,
            format!("the listener answered {status} for the test delivery"),
        ));
    }
    Ok(TestSent {
        delivery,
        status,
        // A forwarding or job-firing route runs no turn of its own.
        runs_turn: !route.deliver_only && route.cron_job.is_none(),
    })
}

#[cfg(feature = "gateway")]
fn test(
    scope: &ProfileScope,
    name: &str,
    payload: Option<&std::path::Path>,
    json: bool,
) -> Result<u8, Error> {
    let body = payload
        .map(|path| {
            std::fs::read(path).map_err(|e| {
                Error::new(
                    &codes::WEBHOOK_TEST_FAILED,
                    format!("cannot read --payload {}: {e}", path.display()),
                )
            })
        })
        .transpose()?;
    let sent = crate::model_cmd::block_on(send_test(scope, name, body))??;
    let TestSent {
        delivery,
        status,
        runs_turn,
    } = sent;
    let session = runs_turn
        .then(|| test_session(scope, name, &delivery))
        .flatten();
    if json {
        return crate::output::json(&json!({
            "route": name, "delivery_id": delivery, "status": status,
            "session_id": session.map(|s| s.to_string()),
        }));
    }
    match session {
        Some(id) => crate::output::print(&format!("delivered {delivery}; session {id}")),
        None => crate::output::print(&format!(
            "delivered {delivery} (status {status}); it started no session"
        )),
    }
}

#[cfg(not(feature = "gateway"))]
fn test(_: &ProfileScope, _: &str, _: Option<&std::path::Path>, _: bool) -> Result<u8, Error> {
    crate::surface::require("gateway").map(|()| 0)
}

/// The session the test delivery started: the gateway routes each
/// delivery to a thread of its own (waits up to 30 s; `None` for a route
/// that runs no turn).
#[cfg(feature = "gateway")]
fn test_session(
    scope: &ProfileScope,
    route: &str,
    delivery: &str,
) -> Option<pmt_core::id::SessionId> {
    let store = pmt_store::Store::open(scope).ok()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let found = store
            .thread_routes("webhook", route)
            .ok()?
            .into_iter()
            .find(|r| r.thread_id.as_deref() == Some(delivery));
        if let Some(found) = found {
            return Some(found.session_id);
        }
        if std::time::Instant::now() > deadline {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

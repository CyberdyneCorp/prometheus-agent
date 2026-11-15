//! The daemon's A2A server (spec: agent-interop): the open Agent-to-Agent
//! protocol (v1.0 and its pre-1.0 aliases) for profiles that set
//! `a2a.server.enabled`, on the daemon-wide listener of `daemon.toml
//! [a2a]` at `/p/<profile>/` (and `/` for `default`).
//!
//! Each inbound task runs as one turn of a session of the profile bound
//! to the exact (peer, tenant, context) of the request, framed as
//! untrusted input from the named peer, with the A2A toolsets and every
//! approval denied; the reply, redacted, becomes the task's artifact.
//! Turns go through the same hub connection and session machinery as the
//! API server's.

mod gate;
mod push;
mod rpc;
mod run;
mod tasks;

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Method, Request};
use hyper_util::rt::{TokioIo, TokioTimer};
use pmt_config::daemon_a2a::A2aListener;
use pmt_config::schema::A2aServerConfig;
use pmt_core::cancel::CancelToken;
use pmt_core::error::Error;
use pmt_gateway::Server;
use serde_json::{Value, json};
use tracing::Instrument as _;

use crate::host::{ApiHost, Catalog};
use crate::http::{Reply, Req, json_reply};
use crate::{Shared, auth, guard, turn};

/// The listener's health component.
const COMPONENT: &str = "listener:a2a";

/// What every A2A request shares.
pub(crate) struct A2a {
    api: Arc<Shared>,
    tasks: tasks::Tasks,
    limits: gate::RateLimits,
    pingpong: gate::PingPong,
    /// One session resolution per conversation key at a time.
    keys: turn::SessionLocks,
    /// The bound address (the card's URL without `public_url`).
    bound: SocketAddr,
}

/// The running A2A server.
pub struct A2aServer {
    addr: SocketAddr,
    stop: CancelToken,
}

impl std::fmt::Debug for A2aServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("A2aServer")
            .field("addr", &self.addr)
            .finish_non_exhaustive()
    }
}

impl A2aServer {
    /// Starts the listener when a served profile enables the A2A server;
    /// `None` otherwise.
    ///
    /// # Errors
    ///
    /// `config.invalid` for a non-loopback address without `a2a.public` or
    /// with a served profile that has no peer token; `net.listen*` when
    /// the listener cannot bind (the health component `listener:a2a` is
    /// then `down`).
    pub async fn start(
        listener: &A2aListener,
        server: Arc<Server>,
        host: Arc<dyn ApiHost>,
    ) -> Result<Option<Self>, Error> {
        let enabled: Vec<(String, A2aServerConfig)> = server
            .profiles()
            .into_iter()
            .filter_map(|p| {
                let config = host.api_scope(&p)?.config().a2a.server.clone();
                config.enabled.then_some((p, config))
            })
            .collect();
        if enabled.is_empty() {
            return Ok(None);
        }
        let addr = listener
            .checked(|| enabled.iter().all(|(_, c)| c.has_tokens()))
            .map_err(|message| Error::new(&pmt_core::error::codes::CONFIG_INVALID, message))?;
        for (profile, config) in &enabled {
            if config.allow_all_peers {
                tracing::warn!(
                    "a2a.server.allow_all_peers admits every authenticated caller of {profile}"
                );
            }
        }
        let exposure = pmt_net::listen::Exposure {
            explicit_address: listener.public,
            authenticated: true,
        };
        let bound = crate::bind(addr, exposure, "a2a server", COMPONENT).await?;
        let addr = bound.local_addr().unwrap_or(addr);
        let api = Shared::new(
            server,
            host,
            guard::Guard::new(&pmt_config::daemon_api::ApiListener::default(), true),
        );
        let a2a = Arc::new(A2a {
            api,
            tasks: tasks::Tasks::default(),
            limits: gate::RateLimits::default(),
            pingpong: gate::PingPong::default(),
            keys: turn::SessionLocks::default(),
            bound: addr,
        });
        let stop = CancelToken::new();
        serve(bound, a2a, stop.clone());
        let names: Vec<&str> = enabled.iter().map(|(p, _)| p.as_str()).collect();
        tracing::info!(
            "a2a server on http://{addr}/ (profiles: {})",
            names.join(", ")
        );
        Ok(Some(Self { addr, stop }))
    }

    /// The bound address.
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Stops accepting connections.
    pub fn stop(&self) {
        self.stop.cancel();
    }
}

impl Drop for A2aServer {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

fn serve(listener: tokio::net::TcpListener, a2a: Arc<A2a>, stop: CancelToken) {
    let slots = Arc::new(tokio::sync::Semaphore::new(crate::MAX_CONNECTIONS));
    pmt_core::spawn::spawn(
        async move {
            loop {
                let accepted = tokio::select! {
                    () = stop.cancelled() => return,
                    accepted = crate::accept(&listener, &slots) => accepted,
                };
                let Some((stream, slot)) = accepted else {
                    continue;
                };
                let caller = stream
                    .peer_addr()
                    .map_or(IpAddr::from([127, 0, 0, 1]), |a| a.ip());
                let a2a = Arc::clone(&a2a);
                pmt_core::spawn::spawn(async move {
                    let _slot = slot;
                    let service = service_fn(move |req| {
                        let a2a = Arc::clone(&a2a);
                        async move { Ok::<_, std::convert::Infallible>(answer(&a2a, req, caller).await) }
                    });
                    let conn = hyper::server::conn::http1::Builder::new()
                        .timer(TokioTimer::new())
                        .header_read_timeout(crate::HEADER_READ_TIMEOUT)
                        .serve_connection(TokioIo::new(stream), service);
                    if let Err(err) = conn.await {
                        tracing::debug!("a2a connection ended: {err}");
                    }
                });
            }
        }
        .in_current_span(),
    );
}

/// The A2A configuration of `profile` when it is served with A2A enabled.
fn served(a2a: &A2a, profile: &str) -> Option<A2aServerConfig> {
    if !a2a.api.server.profiles().iter().any(|p| p == profile) {
        return None;
    }
    let config = a2a.api.host.api_scope(profile)?.config().a2a.server.clone();
    config.enabled.then_some(config)
}

async fn answer(a2a: &Arc<A2a>, request: Request<Incoming>, caller: IpAddr) -> Reply {
    let (parts, body) = request.into_parts();
    let head = Req::head(parts);
    let (profile, path) = auth::split(&head.path);
    let Some(config) = served(a2a, &profile) else {
        return status(404, "not found");
    };
    if !config.has_tokens() {
        let public = config.public_url.as_deref().and_then(gate::url_host);
        if let Err(refusal) = gate::admit_tokenless(&head, public.as_deref()) {
            return refused(&refusal);
        }
    }
    match (&head.method, path.as_str()) {
        (&Method::GET, "/.well-known/agent-card.json" | "/.well-known/agent.json") => {
            card(a2a, &profile, &config, &head).await
        }
        (&Method::POST, "/" | "") => {
            let Some(req) = head.with_body(body).await else {
                return status(413, "the request body is too large");
            };
            rpc::call(a2a, &profile, &config, &req, caller).await
        }
        _ => status(404, "not found"),
    }
}

fn refused(refusal: &gate::Refusal) -> Reply {
    match refusal {
        gate::Refusal::Forbidden(message) => status(403, message),
        gate::Refusal::Unauthorized => json_reply(
            401,
            &pmt_a2a::wire::error(
                &Value::Null,
                pmt_a2a::wire::code::INVALID_REQUEST,
                "invalid or missing bearer token",
            ),
        ),
    }
}

fn status(code: u16, message: &str) -> Reply {
    json_reply(code, &json!({"error": message}))
}

/// The URL peers call: `public_url`, else the forwarded host when trusted,
/// else the bound address (with the profile prefix).
fn callable_url(a2a: &A2a, profile: &str, config: &A2aServerConfig, req: &Req) -> String {
    if let Some(url) = &config.public_url {
        return url.clone();
    }
    let prefix = if profile == "default" {
        "/".to_owned()
    } else {
        format!("/p/{profile}/")
    };
    if config.trust_forwarded_headers
        && let Some(host) = req.header("x-forwarded-host")
    {
        let proto = req.header("x-forwarded-proto").unwrap_or("https");
        return format!("{proto}://{host}{prefix}");
    }
    format!("http://{}{prefix}", a2a.bound)
}

async fn card(a2a: &A2a, profile: &str, config: &A2aServerConfig, req: &Req) -> Reply {
    let skills = match a2a.api.host.api_catalog(profile, Catalog::A2aSkills).await {
        Ok(Value::Array(rows)) => rows
            .iter()
            .map(|row| pmt_a2a::card::Skill {
                id: row["name"].as_str().unwrap_or_default().to_owned(),
                name: row["name"].as_str().unwrap_or_default().to_owned(),
                description: row["description"].as_str().unwrap_or_default().to_owned(),
            })
            .collect(),
        Ok(_) => Vec::new(),
        Err(err) => {
            tracing::warn!("a2a card of {profile}: toolsets unavailable: {err}");
            Vec::new()
        }
    };
    let input = pmt_a2a::card::CardInput {
        name: config
            .agent_name
            .clone()
            .unwrap_or_else(|| format!("{}-{profile}", pmt_core::runtime::hostname())),
        description: config.description.clone().unwrap_or_else(|| {
            format!("Prometheus agent '{profile}': a personal agent with tools, memory and skills.")
        }),
        url: callable_url(a2a, profile, config, req),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        bearer: config.has_tokens(),
        skills,
    };
    json_reply(200, &pmt_a2a::card::card(&input))
}

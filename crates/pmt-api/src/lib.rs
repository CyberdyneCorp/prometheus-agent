//! The daemon's OpenAI-compatible API server (spec: api-server). It lets
//! OpenAI-compatible clients use a profile's whole agent over HTTP: Chat
//! Completions, Responses, a Runs API with approval and clarification
//! controls, and REST over sessions and scheduled jobs.
//!
//! It is opt-in and fails closed: the listener (`daemon.toml
//! [api_server]`) starts only when a profile sets `api_server.enabled`,
//! and a profile is served only with its own bearer key of at least 32
//! characters. Every request is bound to one profile by its `/p/<profile>/`
//! prefix (no prefix: `default`). Run-starting requests are admitted with
//! idempotency keys reserved in the profile's store before any work, and
//! API sessions never take turns in a conversation another surface owns.
//!
//! Turns run in the daemon's own sessions: this crate is a client of the
//! gateway's hub (like the messaging gateway) and asks the daemon for
//! everything else through [`ApiHost`].

// Handlers return `Result<_, Reply>` whose error is the HTTP answer sent
// as is (a hyper response); boxing every refusal would buy nothing.
#![allow(clippy::result_large_err)]

mod admission;
mod auth;
mod chat;
mod content;
mod discovery;
mod guard;
mod host;
mod http;
mod limits;
mod model;
mod peer;
mod responses;
mod responses_out;
mod routes;
mod runs;
mod session;
mod sessions_api;
mod turn;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use pmt_config::daemon_api::ApiListener;
use pmt_core::cancel::CancelToken;
use pmt_core::error::Error;
use pmt_gateway::Server;
use tracing::Instrument as _;

pub use host::{
    ApiHost, ApiImage, ApiTurn, Catalog, JobsCall, ModelChoice, ModelOptions, NewApiSession,
};

/// What every request handler shares.
pub(crate) struct Shared {
    server: Arc<Server>,
    host: Arc<dyn ApiHost>,
    peer: Arc<peer::Peer>,
    locks: turn::SessionLocks,
    guard: guard::Guard,
    limits: limits::Limits,
    inflight: admission::InFlight,
    runs: runs::Runs,
    /// When the daemon began to drain (stop or restart requested).
    draining: Mutex<Option<jiff::Timestamp>>,
}

impl Shared {
    /// The routed profile's store.
    fn store(&self, profile: &str) -> Result<Arc<pmt_store::Store>, http::Reply> {
        self.host
            .api_store(profile)
            .ok_or_else(|| http::not_found("the profile"))
    }

    /// The routed profile's configuration now.
    fn scope(&self, profile: &str) -> Result<pmt_config::ProfileScope, http::Reply> {
        self.host
            .api_scope(profile)
            .ok_or_else(|| http::not_found("the profile"))
    }

    /// When the daemon began to drain, if it does.
    fn draining_since(&self) -> Option<jiff::Timestamp> {
        let mut draining = self.draining.lock().expect("draining poisoned");
        if draining.is_none() && self.server.retiring() {
            *draining = Some(jiff::Timestamp::now());
        }
        *draining
    }
}

/// The running API server.
pub struct ApiServer {
    shared: Arc<Shared>,
    addr: SocketAddr,
    stop: CancelToken,
}

impl std::fmt::Debug for ApiServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiServer")
            .field("addr", &self.addr)
            .finish_non_exhaustive()
    }
}

impl ApiServer {
    /// Starts the listener when a served profile enables the API server;
    /// `None` otherwise. Profiles without a usable key are logged and
    /// answer 404.
    ///
    /// # Errors
    ///
    /// `net.listen_refused` / `net.listen` when the listener cannot bind.
    pub async fn start(
        listener: &ApiListener,
        server: Arc<Server>,
        host: Arc<dyn ApiHost>,
    ) -> Result<Option<Self>, Error> {
        let enabled: Vec<String> = server
            .profiles()
            .into_iter()
            .filter(|p| {
                host.api_scope(p)
                    .is_some_and(|s| s.config().api_server.enabled)
            })
            .collect();
        if enabled.is_empty() {
            return Ok(None);
        }
        for profile in &enabled {
            if auth::profile_key(host.as_ref(), profile).is_none() {
                tracing::error!("api_server disabled for {profile}: missing or weak key");
            }
        }
        let addr = listener
            .listen_addr()
            .map_err(|message| Error::new(&pmt_core::error::codes::CONFIG_PARSE, message))?;
        let loopback = pmt_net::listen::is_loopback(addr.ip());
        if !loopback {
            tracing::warn!(
                "the API server on {addr} is reachable from other hosts (api_server.public = true)"
            );
        }
        let exposure = pmt_net::listen::Exposure {
            explicit_address: listener.public,
            authenticated: true,
        };
        let bound = pmt_net::listen::bind("api server", addr, exposure).await?;
        let addr = bound.local_addr().unwrap_or(addr);
        let peer = peer::Peer::start(&server);
        let shared = Arc::new(Shared {
            guard: guard::Guard::new(listener, loopback),
            runs: runs::Runs::default(),
            limits: limits::Limits::default(),
            inflight: admission::InFlight::default(),
            locks: turn::SessionLocks::default(),
            draining: Mutex::default(),
            server,
            host,
            peer,
        });
        runs::recover(&shared, &enabled);
        let stop = CancelToken::new();
        serve(bound, Arc::clone(&shared), stop.clone());
        tracing::info!(
            "api server on http://{addr}/v1 (profiles: {})",
            enabled.join(", ")
        );
        Ok(Some(Self { shared, addr, stop }))
    }

    /// The bound address.
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The daemon began to drain: run creation and controls answer 503
    /// from now on and runs report `shutdown_requested_at`.
    ///
    /// # Panics
    ///
    /// Never: the lock is only poisoned by a panic while it is held.
    pub fn begin_drain(&self) {
        let mut draining = self.shared.draining.lock().expect("draining poisoned");
        draining.get_or_insert_with(jiff::Timestamp::now);
    }

    /// The drain timed out: every run still active is recorded
    /// `interrupted` before its agent is stopped, so a late result cannot
    /// overwrite it.
    pub fn interrupt_active(&self) {
        runs::interrupt_all(&self.shared);
    }

    /// Stops accepting connections.
    pub fn stop(&self) {
        self.stop.cancel();
    }
}

fn serve(listener: tokio::net::TcpListener, shared: Arc<Shared>, stop: CancelToken) {
    pmt_core::spawn::spawn(
        async move {
            loop {
                let accepted = tokio::select! {
                    () = stop.cancelled() => return,
                    accepted = listener.accept() => accepted,
                };
                let Ok((stream, _)) = accepted else { continue };
                let shared = Arc::clone(&shared);
                pmt_core::spawn::spawn(async move {
                    let service = service_fn(move |req| {
                        let shared = Arc::clone(&shared);
                        async move {
                            Ok::<_, std::convert::Infallible>(routes::answer(&shared, req).await)
                        }
                    });
                    let conn = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service);
                    if let Err(err) = conn.await {
                        tracing::debug!("api connection ended: {err}");
                    }
                });
            }
        }
        .in_current_span(),
    );
}

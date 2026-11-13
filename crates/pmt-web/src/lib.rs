//! The web dashboard (spec: web-dashboard): a browser SPA embedded in the
//! binary, served by the gateway daemon's dashboard listener.
//!
//! The listener is off until `prometheus dashboard` asks the daemon (over
//! its control channel) to open it; it binds loopback by default and fails
//! closed on any other address without real authentication and TLS. The
//! owner signs in through one-time login links; remote users sign in
//! through `dashboard.auth.mode` and observe unless listed as drivers.
//! Chat speaks the client protocol over `/api/rpc` (the same daemon
//! connection every client uses); everything else is JSON under `/api/`,
//! where profile data always lives under `/api/profiles/{profile}/`.
//!
//! - [`Dashboard`]: the daemon's controller of the listener;
//! - [`DashboardHost`]: what the daemon provides (profile scopes, stores,
//!   catalogs, redaction).

// Handlers return `Result<_, Reply>` whose error is the HTTP answer sent
// as is; boxing every refusal would buy nothing.
#![allow(clippy::result_large_err)]

mod api;
mod assets;
mod auth;
mod events;
mod guard;
mod host;
mod http;
mod listener;
mod login;
mod routes;
mod ws;

use std::sync::Arc;

use pmt_config::daemon_dashboard::DashboardConfig;
use pmt_core::error::{Error, codes};
use pmt_gateway::dashboard::DashboardControl;
use pmt_gateway::{HostFuture, Server};
use pmt_rpc::control::{DashboardAction, DashboardState};
use tokio_util::sync::CancellationToken;

pub use host::{Catalog, DashboardHost};

/// What every request handler of a running listener shares.
pub(crate) struct Shared {
    server: Arc<Server>,
    host: Arc<dyn DashboardHost>,
    config: DashboardConfig,
    guard: guard::Guard,
    auth: auth::Auth,
    feed: Arc<events::Feed>,
    /// Cancelled when the listener stops.
    stop: CancellationToken,
    /// The base path without a trailing slash (`""` or `/prometheus`).
    base: String,
}

struct Running {
    shared: Arc<Shared>,
    url: String,
}

/// The daemon's dashboard controller: starts, reports and stops the one
/// listener of this home (spec: web-dashboard, Listener lifecycle within
/// the daemon).
pub struct Dashboard {
    server: Arc<Server>,
    host: Arc<dyn DashboardHost>,
    config: DashboardConfig,
    feed: Arc<events::Feed>,
    running: tokio::sync::Mutex<Option<Running>>,
}

impl std::fmt::Debug for Dashboard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dashboard").finish_non_exhaustive()
    }
}

impl Dashboard {
    /// Installs the controller in `server` (its control channel's
    /// `dashboard` requests reach it) with `daemon.toml [dashboard]`. The
    /// listener stops with the daemon.
    pub fn install(
        server: &Arc<Server>,
        host: Arc<dyn DashboardHost>,
        config: DashboardConfig,
    ) -> Arc<Self> {
        let feed = Arc::new(events::Feed::default());
        let tapped = Arc::clone(&feed);
        server
            .hub()
            .set_tap(pmt_gateway::hub::Tap(Arc::new(move |event| {
                tapped.tap(event);
            })));
        let dashboard = Arc::new(Self {
            server: Arc::clone(server),
            host,
            config,
            feed,
            running: tokio::sync::Mutex::new(None),
        });
        server.set_dashboard(Arc::clone(&dashboard) as Arc<dyn DashboardControl>);
        let retiring = Arc::clone(&dashboard);
        let closing = server.closing();
        pmt_core::spawn::spawn(async move {
            closing.cancelled().await;
            let _ = retiring.stop().await;
        });
        dashboard
    }

    /// Opens the listener unless it runs (`host`/`port` override the
    /// configuration until the daemon stops) and mints a login link.
    ///
    /// # Errors
    ///
    /// `dashboard.bind_refused` (78) for a non-loopback bind without
    /// authentication or TLS, `dashboard.port_in_use` (78), configuration
    /// errors of `dashboard.tls`.
    pub async fn open(
        &self,
        host: Option<&str>,
        port: Option<u16>,
    ) -> Result<DashboardState, Error> {
        let mut running = self.running.lock().await;
        let started = running.is_none();
        let current = match running.take() {
            Some(current) => current,
            None => self.start(host, port).await?,
        };
        let ticket = current.shared.auth.tickets.mint();
        let state = DashboardState {
            url: current.url.clone(),
            login_url: Some(format!("{}login?ticket={ticket}", current.url)),
            started,
        };
        *running = Some(current);
        Ok(state)
    }

    async fn start(&self, host: Option<&str>, port: Option<u16>) -> Result<Running, Error> {
        let config = self.config.with_overrides(host, port);
        config
            .validate()
            .map_err(|message| Error::new(&codes::CONFIG_INVALID, message))?;
        let launch = self.host.launch_profile();
        let secret_set = |name: &str| {
            self.host
                .scope(&launch)
                .is_some_and(|s| s.secrets().resolve(name).is_ok())
        };
        listener::check_remote(&config, secret_set)?;
        let tls = listener::tls(&config)?;
        let socket = listener::bind(&config).await?;
        let bound = socket.local_addr().map_or(config.port, |a| a.port());
        let base = config.base_prefix().to_owned();
        let scheme = if config.has_tls() { "https" } else { "http" };
        let url_host = match config.public_host.as_deref() {
            Some(public) => public.to_owned(),
            None if config.loopback() => format!("127.0.0.1:{bound}"),
            None => format!("{}:{bound}", config.host),
        };
        let shared = Arc::new(Shared {
            guard: guard::Guard::new(&config, bound),
            auth: auth::Auth::new(&config),
            server: Arc::clone(&self.server),
            host: Arc::clone(&self.host),
            feed: Arc::clone(&self.feed),
            stop: CancellationToken::new(),
            base: base.clone(),
            config,
        });
        listener::serve(socket, tls, Arc::clone(&shared));
        let url = format!("{scheme}://{url_host}{base}/");
        tracing::info!("dashboard on {url}");
        Ok(Running { shared, url })
    }

    /// The running listener.
    ///
    /// # Errors
    ///
    /// `dashboard.not_running` (69).
    pub async fn status(&self) -> Result<DashboardState, Error> {
        let running = self.running.lock().await;
        let current = running.as_ref().ok_or_else(not_running)?;
        Ok(DashboardState {
            url: current.url.clone(),
            login_url: None,
            started: false,
        })
    }

    /// Stops the listener, closing its sockets with 1001; the daemon keeps
    /// running.
    ///
    /// # Errors
    ///
    /// `dashboard.not_running` (69).
    pub async fn stop(&self) -> Result<DashboardState, Error> {
        let current = self.running.lock().await.take().ok_or_else(not_running)?;
        current.shared.stop.cancel();
        Ok(DashboardState {
            url: current.url,
            login_url: None,
            started: false,
        })
    }
}

fn not_running() -> Error {
    Error::new(
        &codes::DASHBOARD_NOT_RUNNING,
        "no dashboard listener is running",
    )
    .with_hint("start one with 'prometheus dashboard'")
}

impl DashboardControl for Dashboard {
    fn control(&self, action: DashboardAction) -> HostFuture<'_, DashboardState> {
        Box::pin(async move {
            match action {
                DashboardAction::Open { host, port } => self.open(host.as_deref(), port).await,
                DashboardAction::Status => self.status().await,
                DashboardAction::Stop => self.stop().await,
            }
        })
    }
}

//! The daemon's shared state: identity, tickets, the hub, the host and the
//! router every connection and the control channel use.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use pmt_config::daemon::RpcListenerConfig;
use pmt_core::error::{Error, codes};
use pmt_core::panic::CrashReporter;
use pmt_rpc::Catalog;
use pmt_rpc::contracts::{
    AttachRole, ClientKind, HelloResult, PlatformSupport, ServerCapabilities, Version,
};
use pmt_rpc::control::DaemonStatus;
use pmt_rpc::discovery::Endpoint;
use tokio::sync::{Semaphore, mpsc};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

use crate::host::{Host, Link};
use crate::hub::{ConnId, Hub, Limits, Peer, Violations};
use crate::router::Router;
use crate::tickets::{Grant, TicketBook, random_hex};

/// What the daemon binary is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// Semantic version.
    pub version: String,
    /// Build id (the stale-build check compares it).
    pub build_id: String,
    /// Target triple.
    pub target: String,
    /// Support tier of the target.
    pub tier: String,
}

/// How a daemon is set up.
#[derive(Debug, Clone)]
pub struct ServerOptions {
    /// What the binary is.
    pub identity: Identity,
    /// The profiles it serves.
    pub profiles: Vec<String>,
    /// `[daemon.rpc]`.
    pub rpc: RpcListenerConfig,
    /// The default `approvals.timeout_secs` (hosts pass each profile's).
    pub approvals_timeout: Duration,
    /// Browser origins allowed to upgrade (the dashboard's own origin when
    /// it runs); any other `Origin` is closed with 4403.
    pub allowed_origins: Vec<String>,
    /// The catalog; `None` is this build's.
    pub catalog: Option<Catalog>,
    /// The event-log limits.
    pub limits: Limits,
    /// Abort on a contract violation (debug and test builds) or log and
    /// count it (release builds).
    pub abort_on_violation: bool,
    /// `daemon.idle_session_close_secs`: a session with no client, no
    /// running turn, no open request and no queued input is closed after
    /// this long.
    pub idle_session_close: Duration,
    /// Where a panicking request handler's crash report goes; `None`
    /// keeps none (the request still answers `-32099`).
    pub crash_reporter: Option<Arc<CrashReporter>>,
    /// Tickets only for the client process with this pid (an in-process
    /// gateway that serves its own TUI: `prometheus tui --standalone`).
    pub only_pid: Option<u32>,
}

impl ServerOptions {
    /// The defaults for `identity` serving `profiles`.
    #[must_use]
    pub fn new(identity: Identity, profiles: Vec<String>) -> Self {
        Self {
            identity,
            profiles,
            rpc: RpcListenerConfig::default(),
            approvals_timeout: pmt_core::human::DEFAULT_TIMEOUT,
            allowed_origins: Vec::new(),
            catalog: None,
            limits: Limits::default(),
            abort_on_violation: cfg!(debug_assertions),
            idle_session_close: Duration::from_hours(1),
            crash_reporter: None,
            only_pid: None,
        }
    }
}

/// What the client said in `rpc.hello`.
#[derive(Debug, Clone)]
pub struct HelloState {
    /// The client kind.
    pub client: ClientKind,
    /// Its protocol minor.
    pub minor: u16,
    /// Newer events it understands.
    pub advertised: Arc<Vec<String>>,
    /// It answers server requests.
    pub server_requests: bool,
}

/// One client connection.
#[derive(Debug)]
pub struct Conn {
    /// Its number.
    pub id: ConnId,
    /// What its ticket grants.
    pub grant: Grant,
    /// Its outbound frames.
    pub tx: mpsc::UnboundedSender<Message>,
    /// Set by `rpc.hello`.
    pub hello: OnceLock<HelloState>,
    /// Its ordinary requests, answered in arrival order.
    pub(crate) ordered: mpsc::UnboundedSender<crate::connection::Job>,
}

impl Conn {
    /// How the hub reaches this connection attached as `role`.
    #[must_use]
    pub fn peer(&self, role: AttachRole) -> Peer {
        let hello = self.hello.get();
        Peer {
            tx: self.tx.clone(),
            minor: hello.map_or(0, |h| h.minor),
            advertised: hello.map(|h| Arc::clone(&h.advertised)).unwrap_or_default(),
            server_requests: hello.is_some_and(|h| h.server_requests),
            client_id: self.client_id(),
            role,
        }
    }

    /// The client kind from `rpc.hello`.
    #[must_use]
    pub fn client(&self) -> Option<ClientKind> {
        self.hello.get().map(|h| h.client)
    }

    /// The client's id within the daemon: `<kind>-<connection number>`.
    #[must_use]
    pub fn client_id(&self) -> String {
        let kind = self.client().map_or("client", ClientKind::as_str);
        format!("{kind}-{}", self.id)
    }
}

/// The context of one method call.
#[derive(Debug, Clone)]
pub struct Call {
    /// The daemon.
    pub server: Arc<Server>,
    /// The calling connection.
    pub conn: Arc<Conn>,
}

/// The daemon's shared state.
pub struct Server {
    identity: Identity,
    instance_id: String,
    catalog: Arc<Catalog>,
    digest: String,
    profiles: Vec<String>,
    rpc: RpcListenerConfig,
    approvals_timeout: Duration,
    allowed_origins: Vec<String>,
    pub(crate) tickets: TicketBook,
    hub: Arc<Hub>,
    host: Arc<dyn Host>,
    router: Router,
    violations: Arc<Violations>,
    pub(crate) workers: Arc<Semaphore>,
    retiring: AtomicBool,
    restart: AtomicBool,
    /// A stop was requested: refuse new work and drain.
    pub(crate) stop: CancellationToken,
    /// Draining is over: connections close with 1001.
    pub(crate) closing: CancellationToken,
    pub(crate) connections: AtomicUsize,
    next_conn: AtomicU64,
    started: Instant,
    started_at: String,
    endpoint: OnceLock<Endpoint>,
    idle_session_close: Duration,
    crash_reporter: Option<Arc<CrashReporter>>,
    only_pid: Option<u32>,
    /// Sessions created with an idempotency key: (profile, key) to the
    /// session and when it was created.
    pub(crate) created: tokio::sync::Mutex<HashMap<(String, String), (String, Instant)>>,
    /// Recent admissions by (session, input id), oldest first: a resent
    /// input gets its original admission.
    pub(crate) admitted: tokio::sync::Mutex<std::collections::VecDeque<Admitted>>,
}

/// One remembered `prompt.submit` admission.
pub(crate) type Admitted = ((String, String), pmt_rpc::contracts::Submitted);

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server")
            .field("instance_id", &self.instance_id)
            .field("profiles", &self.profiles)
            .finish_non_exhaustive()
    }
}

impl Server {
    /// The daemon state, with every built-in method registered.
    ///
    /// # Errors
    ///
    /// `daemon.start_failed` when a built-in method is not in the catalog
    /// (an unregistered method name fails at startup).
    pub fn new(options: ServerOptions, host: Arc<dyn Host>) -> Result<Arc<Self>, Error> {
        let catalog = Arc::new(
            options
                .catalog
                .unwrap_or_else(|| Catalog::builtin().clone()),
        );
        let violations = Arc::new(Violations::new(options.abort_on_violation));
        let epoch = random_hex(8);
        let hub = Arc::new(Hub::new(
            epoch,
            Arc::clone(&catalog),
            options.limits,
            Arc::clone(&violations),
        ));
        let mut router = Router::default();
        crate::methods::register(&mut router, &catalog)
            .map_err(|e| Error::new(&codes::DAEMON_START_FAILED, e))?;
        let workers = usize::try_from(options.rpc.workers).unwrap_or(8);
        let server = Self {
            identity: options.identity,
            instance_id: random_hex(16),
            digest: catalog.digest(),
            catalog,
            profiles: options.profiles,
            rpc: options.rpc,
            approvals_timeout: options.approvals_timeout,
            allowed_origins: options.allowed_origins,
            tickets: TicketBook::default(),
            hub,
            host,
            router,
            violations,
            workers: Arc::new(Semaphore::new(workers)),
            retiring: AtomicBool::new(false),
            restart: AtomicBool::new(false),
            stop: CancellationToken::new(),
            closing: CancellationToken::new(),
            connections: AtomicUsize::new(0),
            next_conn: AtomicU64::new(1),
            started: Instant::now(),
            started_at: jiff::Timestamp::now().to_string(),
            endpoint: OnceLock::new(),
            idle_session_close: options.idle_session_close,
            crash_reporter: options.crash_reporter,
            only_pid: options.only_pid,
            created: tokio::sync::Mutex::default(),
            admitted: tokio::sync::Mutex::default(),
        };
        Ok(Arc::new(server))
    }

    /// What the binary is.
    #[must_use]
    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    /// The catalog served.
    #[must_use]
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// The hub.
    #[must_use]
    pub fn hub(&self) -> &Arc<Hub> {
        &self.hub
    }

    /// The host.
    #[must_use]
    pub fn host(&self) -> &Arc<dyn Host> {
        &self.host
    }

    /// The router.
    #[must_use]
    pub fn router(&self) -> &Router {
        &self.router
    }

    /// The violation policy and counter.
    #[must_use]
    pub fn violations(&self) -> &Violations {
        &self.violations
    }

    /// `[daemon.rpc]`.
    #[must_use]
    pub fn rpc_config(&self) -> &RpcListenerConfig {
        &self.rpc
    }

    /// The profiles served.
    #[must_use]
    pub fn profiles(&self) -> &[String] {
        &self.profiles
    }

    /// How long a session may stay without a client before it is closed.
    #[must_use]
    pub fn idle_session_close(&self) -> Duration {
        self.idle_session_close
    }

    /// The only client pid tickets are issued to, when restricted.
    #[must_use]
    pub fn only_pid(&self) -> Option<u32> {
        self.only_pid
    }

    /// Where handler crash reports go.
    #[must_use]
    pub fn crash_reporter(&self) -> Option<&Arc<CrashReporter>> {
        self.crash_reporter.as_ref()
    }

    /// Browser origins allowed to upgrade.
    #[must_use]
    pub fn allowed_origins(&self) -> &[String] {
        &self.allowed_origins
    }

    /// Random per start.
    #[must_use]
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    /// When it started (RFC 3339).
    #[must_use]
    pub fn started_at(&self) -> &str {
        &self.started_at
    }

    /// Records where the daemon listens.
    pub(crate) fn set_endpoint(&self, endpoint: Endpoint) {
        let _ = self.endpoint.set(endpoint);
    }

    /// Where the daemon listens, once bound.
    #[must_use]
    pub fn endpoint(&self) -> Option<&Endpoint> {
        self.endpoint.get()
    }

    /// Draining to stop or restart: new work is refused.
    #[must_use]
    pub fn retiring(&self) -> bool {
        self.retiring.load(Ordering::Relaxed)
    }

    /// Requests a graceful stop (`restart`: the caller starts a new daemon).
    pub fn request_stop(&self, restart: bool) {
        self.retiring.store(true, Ordering::Relaxed);
        if restart {
            self.restart.store(true, Ordering::Relaxed);
        }
        self.stop.cancel();
    }

    /// Whether the stop was for a restart.
    #[must_use]
    pub fn restart_requested(&self) -> bool {
        self.restart.load(Ordering::Relaxed)
    }

    /// A new connection's number.
    pub(crate) fn next_conn_id(&self) -> ConnId {
        self.next_conn.fetch_add(1, Ordering::Relaxed)
    }

    /// The `rpc.hello` result for a connection granted `profiles`.
    #[must_use]
    pub fn hello_result(
        &self,
        profiles: &[String],
        minor: u16,
        advertised: &[String],
    ) -> HelloResult {
        let names = |kind: pmt_rpc::Kind| -> Vec<String> {
            self.catalog
                .entries()
                .filter(|e| e.kind == kind && self.catalog.contract(e.name).is_some())
                .map(|e| e.name.to_owned())
                .collect()
        };
        let events = names(pmt_rpc::Kind::Event)
            .into_iter()
            .filter(|e| self.catalog.event_visible(e, minor, advertised))
            .collect();
        HelloResult {
            server_version: self.identity.version.clone(),
            build_id: self.identity.build_id.clone(),
            instance_id: self.instance_id.clone(),
            epoch: self.hub.epoch().to_owned(),
            protocol: self.catalog.protocol(),
            profiles: profiles.to_vec(),
            methods_digest: self.digest.clone(),
            capabilities: ServerCapabilities {
                methods: self.router().served(),
                server_requests: names(pmt_rpc::Kind::ServerRequest),
                events,
            },
            platform_support: PlatformSupport {
                target: self.identity.target.clone(),
                tier: self.identity.tier.clone(),
            },
        }
    }

    /// The protocol the daemon speaks.
    #[must_use]
    pub fn protocol(&self) -> Version {
        self.catalog.protocol()
    }

    /// A link to `session_id`'s clients.
    #[must_use]
    pub fn link(&self, session_id: &str) -> Link {
        Link::new(
            Arc::clone(&self.hub),
            session_id.to_owned(),
            Duration::from_secs(self.rpc.request_timeout_secs),
            self.approvals_timeout,
        )
    }

    /// `daemon status`.
    #[must_use]
    pub fn status(&self) -> DaemonStatus {
        let endpoint = self.endpoint.get();
        let (sessions, active_turns) = self.host.load();
        DaemonStatus {
            pid: std::process::id(),
            uptime_secs: self.started.elapsed().as_secs(),
            version: self.identity.version.clone(),
            build_id: self.identity.build_id.clone(),
            instance_id: self.instance_id.clone(),
            rpc_url: endpoint.map(|e| e.rpc_url.clone()).unwrap_or_default(),
            rpc_protocol: self.protocol().to_string(),
            profiles: self.profiles.clone(),
            connections: self.connections.load(Ordering::Relaxed),
            sessions,
            active_turns,
            retiring: self.retiring(),
            config: self.host.config(),
        }
    }
}

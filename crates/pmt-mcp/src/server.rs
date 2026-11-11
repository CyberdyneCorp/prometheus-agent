//! One configured MCP server: its connection, restarts, circuit breaker,
//! tool list refreshes and recycling (spec: plugin-system, MCP client
//! servers; MCP config watch, recycling and discovery wait; tool-system,
//! MCP runtime safety; MCP tool list changes respect the frozen surface).
//!
//! The server connects when a session first needs it (never at process
//! start). A connection that is gone (the process exited) or a start that
//! failed leaves the server down: its tools answer `MCP server '<name>'
//! unavailable` and are reported unavailable to new sessions, and the next
//! start is attempted only after a backoff that doubles from 1 s to 60 s.
//! Calls that fail three times in a row open the circuit for 60 seconds:
//! calls are refused without contacting the server until one probe is let
//! through. The tool list of the last successful connect stays known, so
//! the tools stay registered while the server is down.
//!
//! A server stopped on purpose (idle or too old, or `lazy` and never
//! started) is idle: its tools stay offered and the next call starts it.
//! A `tools/list_changed` notification re-lists the tools under a
//! per-server lock, at most once a second, so a burst of notifications
//! costs at most two refreshes; running sessions keep their tool array.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use pmt_config::schema::McpTrust;
use rmcp::model::{CallToolResult, JsonObject, Tool};
use tokio::time::Instant;

use crate::connection::{CallError, Connection, Launch, Offers};
use crate::costs::Costs;
use crate::handler::Handler;
use crate::host::{ClientHost, NoHost};
use crate::naming::Filter;
use crate::requests::{ElicitationPolicy, SamplingPolicy, Window};

/// Consecutive failed calls that open the circuit.
pub const CIRCUIT_FAILURES: u32 = 3;
/// How long an open circuit refuses calls.
pub const CIRCUIT_OPEN: Duration = Duration::from_mins(1);
/// The first restart backoff.
pub const BACKOFF_START: Duration = Duration::from_secs(1);
/// The longest restart backoff.
pub const BACKOFF_MAX: Duration = Duration::from_mins(1);
/// The shortest time between two tool list refreshes.
pub const REFRESH_SPACING: Duration = Duration::from_secs(1);

/// Builds the server's launch for each (re)start.
pub type Launcher = Arc<dyn Fn() -> Result<Launch, String> + Send + Sync>;

/// When a running `stdio` server is stopped on purpose.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Recycle {
    /// After this long without a call.
    pub idle: Option<Duration>,
    /// This long after it started.
    pub max_lifetime: Option<Duration>,
}

/// How a server is used.
#[derive(Debug, Clone)]
pub struct Settings {
    /// `stdio` or `http` (for status).
    pub transport: &'static str,
    /// `full` or `untrusted`.
    pub trust: McpTrust,
    /// Which tools register.
    pub filter: Filter,
    /// Bound on one call.
    pub call_timeout: Duration,
    /// Bound on a start (spawn, `initialize`, `tools/list`).
    pub connect_timeout: Duration,
    /// `sampling/createMessage` answers.
    pub sampling: SamplingPolicy,
    /// `elicitation/create` answers.
    pub elicitation: ElicitationPolicy,
    /// What its tools cost.
    pub costs: Costs,
    /// Idle and lifetime stops.
    pub recycle: Recycle,
    /// Offer the cached tool list and start on the first call.
    pub lazy: bool,
    /// Where the tool list is cached between processes.
    pub schema_cache: Option<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            transport: "stdio",
            trust: McpTrust::Untrusted,
            filter: Filter::default(),
            call_timeout: Duration::from_mins(5),
            connect_timeout: Duration::from_mins(1),
            sampling: SamplingPolicy::default(),
            elicitation: ElicitationPolicy::default(),
            costs: Costs::default(),
            recycle: Recycle::default(),
            lazy: false,
            schema_cache: None,
        }
    }
}

/// Why the server is down and when it may start again.
#[derive(Debug, Clone)]
struct Down {
    reason: String,
    attempts: u32,
    retry_at: Instant,
}

#[derive(Debug, Default)]
struct Breaker {
    failures: u32,
    open_until: Option<Instant>,
}

#[derive(Default)]
struct Shared {
    connection: Option<Arc<Connection>>,
    /// Bumped at each start (so a recycling timer knows its connection).
    connection_seq: u64,
    started_at: Option<Instant>,
    last_used: Option<Instant>,
    in_flight: u32,
    /// Stopped on purpose; the next call starts it.
    idle: bool,
    tools: Vec<Tool>,
    offers: Offers,
    down: Option<Down>,
    breaker: Breaker,
    /// Sessions with a call in flight, the newest last.
    callers: Vec<String>,
    /// Bumped whenever the tool list changes.
    epoch: u64,
}

/// A process-wide serial number (server and manager identities).
#[must_use]
pub fn next_serial() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// One configured server.
pub struct Server {
    name: String,
    serial: u64,
    settings: Settings,
    launcher: Launcher,
    host: Arc<dyn ClientHost>,
    /// Serializes starts.
    starting: tokio::sync::Mutex<()>,
    /// Serializes tool list refreshes.
    refreshing: tokio::sync::Mutex<()>,
    refresh_pending: AtomicBool,
    refreshes: AtomicU64,
    pub(crate) sampling_window: Mutex<Window>,
    shared: Mutex<Shared>,
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// What `mcp status` shows of a server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// Connected.
    Connected,
    /// Stopped on purpose (idle, too old, lazy): its tools are offered
    /// and the next call starts it.
    Idle,
    /// Not started yet.
    NotConnected,
    /// Down, with the reason.
    Unavailable(String),
}

impl Server {
    /// A server named `name`, started with `launcher` when needed, routing
    /// session requests to `host`.
    #[must_use]
    pub fn new(
        name: &str,
        settings: Settings,
        launcher: Launcher,
        host: Arc<dyn ClientHost>,
    ) -> Self {
        let mut shared = Shared::default();
        if let Some((tools, offers)) = settings
            .schema_cache
            .as_deref()
            .and_then(crate::cache::read)
        {
            shared.tools = tools;
            shared.offers = offers;
            shared.idle = settings.lazy;
        }
        Self {
            name: name.to_owned(),
            serial: next_serial(),
            settings,
            launcher,
            host,
            starting: tokio::sync::Mutex::new(()),
            refreshing: tokio::sync::Mutex::new(()),
            refresh_pending: AtomicBool::new(false),
            refreshes: AtomicU64::new(0),
            sampling_window: Mutex::new(Window::default()),
            shared: Mutex::new(shared),
        }
    }

    /// A server without a host (tests).
    #[must_use]
    pub fn standalone(name: &str, settings: Settings, launcher: Launcher) -> Self {
        Self::new(name, settings, launcher, Arc::new(NoHost))
    }

    /// The configured name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// How it is used.
    #[must_use]
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// The program's side.
    #[must_use]
    pub fn host(&self) -> &Arc<dyn ClientHost> {
        &self.host
    }

    fn lock(&self) -> MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// This server object's identity (a rebuilt server gets another).
    #[must_use]
    pub fn serial(&self) -> u64 {
        self.serial
    }

    /// Bumped whenever the tool list changes.
    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.lock().epoch
    }

    /// The tools the server listed last.
    #[must_use]
    pub fn tools(&self) -> Vec<Tool> {
        self.lock().tools.clone()
    }

    /// What it offers besides tools, as of its last start.
    #[must_use]
    pub fn offers(&self) -> Offers {
        self.lock().offers
    }

    /// Tool list refreshes run so far (after `tools/list_changed`).
    #[must_use]
    pub fn refreshes(&self) -> u64 {
        self.refreshes.load(Ordering::Relaxed)
    }

    /// The session of the newest call in flight, if any.
    #[must_use]
    pub fn caller(&self) -> Option<String> {
        self.lock().callers.last().cloned()
    }

    /// Whether its tools may be offered to a new session: connected or
    /// idle, or the reason they may not.
    ///
    /// # Errors
    ///
    /// The reason (`mcp_server_down: ...`).
    pub fn availability(&self) -> Result<(), String> {
        match self.state() {
            State::Connected | State::Idle => Ok(()),
            State::NotConnected => Err("mcp_server_not_connected".to_owned()),
            State::Unavailable(reason) => Err(format!("mcp_server_down: {reason}")),
        }
    }

    /// The server's state now (a connection found gone counts as down).
    #[must_use]
    pub fn state(&self) -> State {
        let mut shared = self.lock();
        if shared.connection.as_ref().is_some_and(|c| c.is_closed()) {
            mark_crashed(&mut shared);
        }
        match (&shared.connection, &shared.down) {
            (Some(_), _) => State::Connected,
            (None, Some(down)) => State::Unavailable(down.reason.clone()),
            (None, None) if shared.idle => State::Idle,
            (None, None) => State::NotConnected,
        }
    }

    /// Starts the server unless it is connected, idle (stopped on
    /// purpose) or its restart backoff is still running. Runs on the MCP
    /// runtime.
    ///
    /// # Errors
    ///
    /// Why it is down.
    pub async fn ensure(self: &Arc<Self>) -> Result<(), String> {
        let this = Arc::clone(self);
        crate::runtime::run(async move { this.ensure_here(false).await.map(|_| ()) }).await
    }

    /// Starts the server even when idle (`mcp test`).
    ///
    /// # Errors
    ///
    /// Why it is down.
    pub async fn connect(self: &Arc<Self>) -> Result<(), String> {
        let this = Arc::clone(self);
        crate::runtime::run(async move { this.ensure_here(true).await.map(|_| ()) }).await
    }

    async fn ensure_here(self: &Arc<Self>, wake: bool) -> Result<Option<Arc<Connection>>, String> {
        let _starting = self.starting.lock().await;
        {
            let mut shared = self.lock();
            if let Some(connection) = &shared.connection {
                if !connection.is_closed() {
                    return Ok(Some(Arc::clone(connection)));
                }
                mark_crashed(&mut shared);
            }
            if shared.idle && !wake {
                return Ok(None);
            }
            if let Some(down) = &shared.down
                && Instant::now() < down.retry_at
            {
                return Err(down.reason.clone());
            }
        }
        let handler = Handler::new(
            Arc::downgrade(self),
            self.settings.sampling.enabled,
            self.settings.elicitation.enabled,
        );
        let opened = match (self.launcher)() {
            Ok(launch) => Connection::open(launch, handler, self.settings.connect_timeout).await,
            Err(err) => Err(err),
        };
        match opened {
            Ok(connection) => Ok(Some(self.connected(connection))),
            Err(reason) => {
                tracing::warn!(server = %self.name, "MCP server could not start: {reason}");
                mark_down(&mut self.lock(), reason.clone());
                Err(reason)
            }
        }
    }

    fn connected(self: &Arc<Self>, connection: Connection) -> Arc<Connection> {
        let connection = Arc::new(connection);
        let seq = {
            let mut shared = self.lock();
            shared.offers = connection.offers();
            self.warn_unknown_costs(connection.tools());
            self.adopt_tools(&mut shared, connection.tools().to_vec());
            shared.connection = Some(Arc::clone(&connection));
            shared.connection_seq += 1;
            shared.started_at = Some(Instant::now());
            shared.last_used = Some(Instant::now());
            shared.idle = false;
            shared.down = None;
            shared.connection_seq
        };
        if self.settings.recycle != Recycle::default() {
            pmt_core::spawn::spawn(recycle(Arc::downgrade(self), seq));
        }
        connection
    }

    /// Warns about `tool_costs` entries naming tools the server does not
    /// offer.
    fn warn_unknown_costs(&self, listed: &[Tool]) {
        for tool in self.settings.costs.tools.keys() {
            if !listed.iter().any(|t| t.name == tool.as_str()) {
                tracing::warn!(server = %self.name, "tool_costs.{tool}: MCP server '{}' offers no tool '{tool}'", self.name);
            }
        }
    }

    /// Takes `listed` as the tool list (caching it); answers whether it
    /// changed.
    fn adopt_tools(&self, shared: &mut Shared, listed: Vec<Tool>) -> bool {
        if let Some(path) = &self.settings.schema_cache {
            crate::cache::write(path, &listed, shared.offers);
        }
        if shared.tools == listed {
            return false;
        }
        shared.tools = listed;
        shared.epoch += 1;
        true
    }

    /// Calls the server's tool `tool` (its own name) for `session`. Runs
    /// on the MCP runtime.
    ///
    /// # Errors
    ///
    /// The text the model gets: the server is unavailable, its circuit is
    /// open, the tool is no longer offered, the call timed out or failed.
    pub async fn call(
        self: &Arc<Self>,
        session: &str,
        tool: &str,
        arguments: JsonObject,
    ) -> Result<CallToolResult, String> {
        let this = Arc::clone(self);
        let (tool, session) = (tool.to_owned(), session.to_owned());
        crate::runtime::run(async move {
            if !this.lock().tools.iter().any(|t| t.name == tool) {
                return Err(format!(
                    "tool '{}' is no longer offered by MCP server '{}'",
                    crate::naming::tool_name(&this.name, &tool),
                    this.name
                ));
            }
            let timeout = this.settings.call_timeout;
            let what = tool.clone();
            this.with_connection(&session, &what, move |c| async move {
                c.call(&tool, arguments, timeout).await
            })
            .await
        })
        .await
    }

    /// Runs `request` on the connection (started when needed) for
    /// `session`, through the circuit breaker; `what` names it in errors.
    /// Runs on the MCP runtime.
    pub(crate) async fn with_connection<T, F, Fut>(
        self: &Arc<Self>,
        session: &str,
        what: &str,
        request: F,
    ) -> Result<T, String>
    where
        F: FnOnce(Arc<Connection>) -> Fut,
        Fut: Future<Output = Result<T, CallError>>,
    {
        self.admit()?;
        let connection = match self.ensure_here(true).await {
            Ok(Some(connection)) => connection,
            Ok(None) => return Err(self.unavailable("it did not start")),
            Err(reason) => {
                self.failed();
                return Err(self.unavailable(&reason));
            }
        };
        self.begin(session);
        let outcome = request(connection).await;
        self.end(session);
        self.settle(what, outcome)
    }

    fn begin(&self, session: &str) {
        let mut shared = self.lock();
        shared.callers.push(session.to_owned());
        shared.in_flight += 1;
        shared.last_used = Some(Instant::now());
    }

    fn end(&self, session: &str) {
        let mut shared = self.lock();
        if let Some(at) = shared.callers.iter().rposition(|s| s == session) {
            shared.callers.remove(at);
        }
        shared.in_flight = shared.in_flight.saturating_sub(1);
        shared.last_used = Some(Instant::now());
    }

    fn settle<T>(&self, what: &str, outcome: Result<T, CallError>) -> Result<T, String> {
        match outcome {
            Ok(value) => {
                self.lock().breaker = Breaker::default();
                Ok(value)
            }
            Err(CallError::Failed(message)) => {
                // The server answered: it is reachable.
                self.lock().breaker = Breaker::default();
                Err(format!(
                    "MCP server '{}': {what} failed: {message}",
                    self.name
                ))
            }
            Err(CallError::Closed) => {
                mark_crashed(&mut self.lock());
                self.failed();
                Err(self.unavailable("the server process exited"))
            }
            Err(CallError::TimedOut(after)) => {
                self.failed();
                Err(format!(
                    "MCP server '{}' did not answer {what} within {}",
                    self.name,
                    crate::connection::span(after)
                ))
            }
        }
    }

    fn unavailable(&self, reason: &str) -> String {
        format!("MCP server '{}' unavailable: {reason}", self.name)
    }

    /// The circuit check: refused while open; once the open period is
    /// over one probe goes through (the circuit re-arms until it answers).
    fn admit(&self) -> Result<(), String> {
        let mut shared = self.lock();
        let now = Instant::now();
        let breaker = &mut shared.breaker;
        let Some(until) = breaker.open_until else {
            return Ok(());
        };
        if now >= until {
            breaker.open_until = Some(now + CIRCUIT_OPEN);
            return Ok(());
        }
        let left = (until - now).as_secs_f64().ceil();
        Err(format!(
            "MCP server '{}' is unreachable after {} consecutive failures; retry in ~{left}s",
            self.name, breaker.failures
        ))
    }

    fn failed(&self) {
        let mut shared = self.lock();
        let breaker = &mut shared.breaker;
        breaker.failures += 1;
        if breaker.failures >= CIRCUIT_FAILURES {
            breaker.open_until = Some(Instant::now() + CIRCUIT_OPEN);
        }
    }

    /// `notifications/tools/list_changed`: re-lists the tools, coalescing
    /// notifications that arrive during a refresh into one follow-up.
    pub(crate) async fn tools_list_changed(self: Arc<Self>) {
        self.refresh_pending.store(true, Ordering::SeqCst);
        let mut last: Option<Instant> = None;
        loop {
            let Ok(refreshing) = self.refreshing.try_lock() else {
                return;
            };
            loop {
                // Wait out the spacing before taking the pending flag, so
                // every notification of the wait is served by one refresh.
                if let Some(last) = last {
                    tokio::time::sleep_until(last + REFRESH_SPACING).await;
                }
                if !self.refresh_pending.swap(false, Ordering::SeqCst) {
                    break;
                }
                last = Some(Instant::now());
                self.refresh().await;
            }
            drop(refreshing);
            if !self.refresh_pending.load(Ordering::SeqCst) {
                return;
            }
        }
    }

    async fn refresh(&self) {
        self.refreshes.fetch_add(1, Ordering::Relaxed);
        let connection = self.lock().connection.clone();
        let Some(connection) = connection else {
            return;
        };
        match connection.list_tools(self.settings.call_timeout).await {
            Ok(listed) => {
                let changed = self.adopt_tools(&mut self.lock(), listed);
                if changed {
                    tracing::info!(server = %self.name, "MCP server's tools changed; new sessions get them");
                    self.host.tools_changed(&self.name);
                }
            }
            Err(err) => {
                tracing::warn!(server = %self.name, "tools/list after list_changed failed: {err:?}");
            }
        }
    }

    /// Stops the server (its process ends with the connection).
    pub fn stop(&self) {
        self.lock().connection = None;
    }

    /// When the connection numbered `seq` is due to stop, or `None` when
    /// it is gone or never stops.
    fn recycle_deadline(&self, seq: u64) -> Option<Instant> {
        let shared = self.lock();
        if shared.connection_seq != seq || shared.connection.is_none() {
            return None;
        }
        let recycle = self.settings.recycle;
        let idle = recycle.idle.zip(shared.last_used).map(|(d, at)| at + d);
        let old = recycle
            .max_lifetime
            .zip(shared.started_at)
            .map(|(d, at)| at + d);
        match (idle, old) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (one, other) => one.or(other),
        }
    }

    /// Stops the connection numbered `seq` when it is due and no call is
    /// in flight; answers whether it stopped (or is gone).
    fn recycle_now(&self, seq: u64) -> bool {
        let mut shared = self.lock();
        if shared.connection_seq != seq || shared.connection.is_none() {
            return true;
        }
        let now = Instant::now();
        let recycle = self.settings.recycle;
        let due = recycle
            .idle
            .zip(shared.last_used)
            .is_some_and(|(d, at)| now >= at + d)
            || recycle
                .max_lifetime
                .zip(shared.started_at)
                .is_some_and(|(d, at)| now >= at + d);
        if !due || shared.in_flight > 0 {
            return false;
        }
        tracing::info!(server = %self.name, "MCP server stopped (recycled); it restarts on its next call");
        shared.connection = None;
        shared.idle = true;
        true
    }
}

/// Stops the connection numbered `seq` once it has been idle or alive too
/// long (waiting for a call in flight to end).
async fn recycle(server: Weak<Server>, seq: u64) {
    loop {
        let deadline = match server.upgrade() {
            Some(this) => this.recycle_deadline(seq),
            None => None,
        };
        let Some(deadline) = deadline else { return };
        tokio::time::sleep_until(deadline.max(Instant::now() + Duration::from_millis(10))).await;
        match server.upgrade() {
            Some(this) if this.recycle_now(seq) => return,
            Some(_) => {}
            None => return,
        }
    }
}

/// The connection is gone: down, first restart after [`BACKOFF_START`].
fn mark_crashed(shared: &mut Shared) {
    shared.connection = None;
    shared.down = Some(Down {
        reason: "the server process exited".to_owned(),
        attempts: 1,
        retry_at: Instant::now() + BACKOFF_START,
    });
}

/// A start failed: the backoff doubles up to [`BACKOFF_MAX`].
fn mark_down(shared: &mut Shared, reason: String) {
    let attempts = shared.down.as_ref().map_or(1, |d| d.attempts + 1);
    shared.down = Some(Down {
        reason,
        attempts,
        retry_at: Instant::now() + backoff(attempts),
    });
}

/// The wait before restart attempt `attempts + 1`: 1 s, 2 s, 4 s ... 60 s.
#[must_use]
pub fn backoff(attempts: u32) -> Duration {
    let doublings = attempts.saturating_sub(1).min(6);
    (BACKOFF_START * 2u32.pow(doublings)).min(BACKOFF_MAX)
}

#[cfg(test)]
mod tests {
    use super::backoff;
    use std::time::Duration;

    #[test]
    fn restart_backoff_doubles_from_one_second_to_sixty() {
        let waits: Vec<u64> = (1..=8).map(|n| backoff(n).as_secs()).collect();
        assert_eq!(waits, [1, 2, 4, 8, 16, 32, 60, 60]);
        assert_eq!(backoff(0), Duration::from_secs(1));
    }
}

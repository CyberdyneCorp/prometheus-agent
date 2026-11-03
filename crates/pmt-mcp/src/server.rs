//! One configured MCP server: its connection, restarts and circuit breaker
//! (spec: plugin-system, MCP client servers; tool-system, MCP runtime
//! safety).
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

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use pmt_config::schema::McpTrust;
use rmcp::model::{CallToolResult, JsonObject, Tool};
use tokio::time::Instant;

use crate::connection::{CallError, Connection, Launch};
use crate::naming::Filter;

/// Consecutive failed calls that open the circuit.
pub const CIRCUIT_FAILURES: u32 = 3;
/// How long an open circuit refuses calls.
pub const CIRCUIT_OPEN: Duration = Duration::from_mins(1);
/// The first restart backoff.
pub const BACKOFF_START: Duration = Duration::from_secs(1);
/// The longest restart backoff.
pub const BACKOFF_MAX: Duration = Duration::from_mins(1);

/// Builds the server's launch for each (re)start.
pub type Launcher = Arc<dyn Fn() -> Result<Launch, String> + Send + Sync>;

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
    tools: Vec<Tool>,
    down: Option<Down>,
    breaker: Breaker,
    /// Bumped whenever a start lists different tools.
    epoch: u64,
}

/// One configured server.
pub struct Server {
    name: String,
    settings: Settings,
    launcher: Launcher,
    /// Serializes starts.
    starting: tokio::sync::Mutex<()>,
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
    /// Not started yet.
    NotConnected,
    /// Down, with the reason.
    Unavailable(String),
}

impl Server {
    /// A server named `name`, started with `launcher` when needed.
    #[must_use]
    pub fn new(name: &str, settings: Settings, launcher: Launcher) -> Self {
        Self {
            name: name.to_owned(),
            settings,
            launcher,
            starting: tokio::sync::Mutex::new(()),
            shared: Mutex::new(Shared::default()),
        }
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

    fn lock(&self) -> MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Bumped whenever a start lists different tools.
    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.lock().epoch
    }

    /// The tools the server listed at its last successful start.
    #[must_use]
    pub fn tools(&self) -> Vec<Tool> {
        self.lock().tools.clone()
    }

    /// Whether its tools may be offered to a new session: connected, or
    /// the reason they may not.
    ///
    /// # Errors
    ///
    /// The reason (`mcp_server_down: ...`).
    pub fn availability(&self) -> Result<(), String> {
        match self.state() {
            State::Connected => Ok(()),
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
            (None, None) => State::NotConnected,
        }
    }

    /// Starts the server unless it is connected or its restart backoff is
    /// still running. Runs on the MCP runtime.
    ///
    /// # Errors
    ///
    /// Why it is down.
    pub async fn ensure(self: &Arc<Self>) -> Result<(), String> {
        let this = Arc::clone(self);
        crate::runtime::run(async move { this.ensure_here().await.map(|_| ()) }).await
    }

    async fn ensure_here(&self) -> Result<Arc<Connection>, String> {
        let _starting = self.starting.lock().await;
        {
            let mut shared = self.lock();
            if let Some(connection) = &shared.connection {
                if !connection.is_closed() {
                    return Ok(Arc::clone(connection));
                }
                mark_crashed(&mut shared);
            }
            if let Some(down) = &shared.down
                && Instant::now() < down.retry_at
            {
                return Err(down.reason.clone());
            }
        }
        let opened = match (self.launcher)() {
            Ok(launch) => Connection::open(launch, self.settings.connect_timeout).await,
            Err(err) => Err(err),
        };
        let mut shared = self.lock();
        match opened {
            Ok(connection) => {
                let connection = Arc::new(connection);
                if shared.tools != connection.tools() {
                    shared.tools = connection.tools().to_vec();
                    shared.epoch += 1;
                }
                shared.connection = Some(Arc::clone(&connection));
                shared.down = None;
                Ok(connection)
            }
            Err(reason) => {
                tracing::warn!(server = %self.name, "MCP server could not start: {reason}");
                mark_down(&mut shared, reason.clone());
                Err(reason)
            }
        }
    }

    /// Calls the server's tool `tool` (its own name). Runs on the MCP
    /// runtime.
    ///
    /// # Errors
    ///
    /// The text the model gets: the server is unavailable, its circuit is
    /// open, the call timed out or failed.
    pub async fn call(
        self: &Arc<Self>,
        tool: &str,
        arguments: JsonObject,
    ) -> Result<CallToolResult, String> {
        let this = Arc::clone(self);
        let tool = tool.to_owned();
        crate::runtime::run(async move { this.call_here(&tool, arguments).await }).await
    }

    async fn call_here(&self, tool: &str, arguments: JsonObject) -> Result<CallToolResult, String> {
        self.admit()?;
        let connection = match self.ensure_here().await {
            Ok(connection) => connection,
            Err(reason) => {
                self.failed();
                return Err(self.unavailable(&reason));
            }
        };
        let timeout = self.settings.call_timeout;
        match connection.call(tool, arguments, timeout).await {
            Ok(result) => {
                self.lock().breaker = Breaker::default();
                Ok(result)
            }
            Err(CallError::Failed(message)) => {
                // The server answered: it is reachable.
                self.lock().breaker = Breaker::default();
                Err(format!(
                    "MCP server '{}': {tool} failed: {message}",
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
                    "MCP server '{}' did not answer {tool} within {}",
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

    /// Stops the server (its process ends with the connection).
    pub fn stop(&self) {
        self.lock().connection = None;
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

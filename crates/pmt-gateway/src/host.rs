//! What the daemon serves sessions with: the [`Host`] runs agents, turns
//! and session storage for every served profile (the binary implements it
//! over the agent; tests use fakes), and reaches the session's clients
//! only through its [`Link`].

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use pmt_core::error::Error;
use pmt_rpc::contracts::{
    CompactParams, Compacted, ContextUsage, Event, ForkParams, ResolveParams, ServerRequest,
    SessionCreateParams, SessionList, SessionListParams, SessionRef, SubmitParams, Submitted,
    Transcript,
};

use crate::hub::Hub;

/// A boxed host future.
pub type HostFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, Error>> + Send + 'a>>;

/// The agent side of the daemon. Every call names the profile the
/// connection's ticket grants; the daemon has already checked that the
/// session belongs to it.
pub trait Host: Send + Sync + 'static {
    /// Whether `session_id` is a session of `profile`. Malformed ids and
    /// other profiles' sessions are `false`, never an error, so callers
    /// cannot tell them apart.
    fn owns<'a>(&'a self, profile: &'a str, session_id: &'a str) -> HostFuture<'a, bool>;

    /// Creates a session (loaded on its first turn or resume).
    fn create<'a>(
        &'a self,
        profile: &'a str,
        params: SessionCreateParams,
    ) -> HostFuture<'a, SessionRef>;

    /// The profile's sessions.
    fn list<'a>(
        &'a self,
        profile: &'a str,
        params: SessionListParams,
    ) -> HostFuture<'a, SessionList>;

    /// Loads an existing session (a no-op when it is loaded).
    fn resume<'a>(&'a self, profile: &'a str, link: Link) -> HostFuture<'a, ()>;

    /// The session's active messages.
    fn transcript<'a>(
        &'a self,
        profile: &'a str,
        session_id: &'a str,
    ) -> HostFuture<'a, Transcript>;

    /// The session's context size.
    fn context_usage<'a>(
        &'a self,
        profile: &'a str,
        session_id: &'a str,
    ) -> HostFuture<'a, ContextUsage>;

    /// Compacts the session's context now.
    fn compact<'a>(&'a self, profile: &'a str, params: CompactParams) -> HostFuture<'a, Compacted>;

    /// Starts a turn; its output goes through `link` and it ends with a
    /// `turn.finished` event.
    fn submit<'a>(
        &'a self,
        profile: &'a str,
        params: SubmitParams,
        link: Link,
    ) -> HostFuture<'a, Submitted>;

    /// Stops the session's running turn at its next safe point; a no-op
    /// when none runs.
    fn interrupt<'a>(&'a self, profile: &'a str, session_id: &'a str) -> HostFuture<'a, ()>;

    /// The session a chat's `-r`/`-c` reference names.
    fn resolve<'a>(&'a self, profile: &'a str, params: ResolveParams)
    -> HostFuture<'a, SessionRef>;

    /// Forks a session (the parent is unchanged).
    fn fork<'a>(&'a self, profile: &'a str, params: ForkParams) -> HostFuture<'a, SessionRef>;

    /// Sessions loaded and turns running, for `daemon status`.
    fn load(&self) -> (usize, usize);

    /// Waits until no turn runs, at most `deadline` (a restart drains to a
    /// turn boundary).
    fn drain(&self, deadline: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}

/// A session's way to its attached clients.
#[derive(Debug, Clone)]
pub struct Link {
    hub: Arc<Hub>,
    session_id: String,
    request_timeout: Duration,
    approval_timeout: Duration,
}

impl Link {
    pub(crate) fn new(
        hub: Arc<Hub>,
        session_id: String,
        request_timeout: Duration,
        approval_timeout: Duration,
    ) -> Self {
        Self {
            hub,
            session_id,
            request_timeout,
            approval_timeout,
        }
    }

    /// The session.
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Emits event `E` on the session.
    pub fn emit<E: Event>(&self, payload: &E::Payload) {
        let data = serde_json::to_value(payload).unwrap_or_default();
        self.hub.publish(&self.session_id, E::NAME, data);
    }

    /// Asks the session's capable clients; `None` is unanswered (callers
    /// deny). Approvals wait `approvals_timeout`, the rest the daemon's
    /// request timeout.
    pub async fn ask<R: ServerRequest>(
        &self,
        params: &R::Params,
        approvals_timeout: Option<Duration>,
    ) -> Option<R::Result> {
        let params = serde_json::to_value(params).ok()?;
        let timeout = if R::NAME == "approval.request" {
            approvals_timeout.unwrap_or(self.approval_timeout)
        } else {
            self.request_timeout
        };
        let answer = self
            .hub
            .ask(&self.session_id, R::NAME, params, timeout)
            .await?;
        serde_json::from_value(answer).ok()
    }
}

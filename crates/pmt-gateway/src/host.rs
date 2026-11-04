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
    BannerInfo, ClientKind, CommandCatalog, CompactParams, Compacted, CompleteParams, Completions,
    ConfigValue, ContextUsage, DispatchParams, Dispatched, EgressSetParams, EgressState, Event,
    ForkParams, ModelOptions, ResolveParams, ServerRequest, SessionCreateParams, SessionList,
    SessionListParams, SessionRef, SessionSetup, SubagentList, SubmitParams, Submitted, Transcript,
};
use serde::Serialize;

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

    /// Loads an existing session (a no-op when it is loaded, unless
    /// `setup` asks for another one: an idle session is then reloaded).
    fn resume<'a>(
        &'a self,
        profile: &'a str,
        setup: Option<SessionSetup>,
        link: Link,
    ) -> HostFuture<'a, ()>;

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

    /// Admits an input stamped with `author`: starts its turn, or, while
    /// one runs, queues, steers or interrupts per `busy.input_mode` (spec:
    /// tui, Prompt submission while busy). A turn's output goes through
    /// `link` and it ends with a `turn.finished` event.
    fn submit<'a>(
        &'a self,
        profile: &'a str,
        params: SubmitParams,
        author: Author,
        link: Link,
    ) -> HostFuture<'a, Submitted>;

    /// Runs a slash command in the session (`command.dispatch`); its
    /// notices and any turn it starts reach the clients through `link`.
    fn dispatch<'a>(
        &'a self,
        profile: &'a str,
        params: DispatchParams,
        link: Link,
    ) -> HostFuture<'a, Dispatched>;

    /// Changes (or reads) the session's egress policy (`egress.set`),
    /// loading the session first. Hosts without egress policies refuse.
    fn egress<'a>(
        &'a self,
        profile: &'a str,
        params: EgressSetParams,
        link: Link,
    ) -> HostFuture<'a, EgressState> {
        let _ = (profile, params, link);
        Box::pin(async {
            Err(Error::new(
                &pmt_core::error::codes::RPC_NOT_SERVED,
                "this host has no session egress policies",
            ))
        })
    }

    /// Stops the session's running turn at its next safe point; a no-op
    /// when none runs.
    fn interrupt<'a>(&'a self, profile: &'a str, session_id: &'a str) -> HostFuture<'a, ()>;

    /// The session a chat's `-r`/`-c` reference names.
    fn resolve<'a>(&'a self, profile: &'a str, params: ResolveParams)
    -> HostFuture<'a, SessionRef>;

    /// Forks a session (the parent is unchanged).
    fn fork<'a>(&'a self, profile: &'a str, params: ForkParams) -> HostFuture<'a, SessionRef>;

    /// The session's slash commands, with their disposition for `client`
    /// (`commands.catalog`). Never changes the live session.
    fn commands<'a>(
        &'a self,
        profile: &'a str,
        session_id: &'a str,
        client: ClientKind,
    ) -> HostFuture<'a, CommandCatalog>;

    /// Entries for a typed path, listed through the session's terminal
    /// backend; empty, never an error, when the backend is unreachable.
    fn complete_path<'a>(
        &'a self,
        profile: &'a str,
        params: CompleteParams,
    ) -> HostFuture<'a, Completions>;

    /// The models the session may switch to.
    fn model_options<'a>(
        &'a self,
        profile: &'a str,
        session_id: &'a str,
    ) -> HostFuture<'a, ModelOptions>;

    /// The session's delegated children (none until a host delegates).
    fn subagents<'a>(
        &'a self,
        _profile: &'a str,
        _session_id: &'a str,
    ) -> HostFuture<'a, SubagentList> {
        Box::pin(async { Ok(SubagentList::default()) })
    }

    /// What the session's startup banner lists (`session.banner`). Never
    /// rebuilds its prompt; nothing listed by a host that knows none.
    fn banner<'a>(&'a self, _profile: &'a str, _session_id: &'a str) -> HostFuture<'a, BannerInfo> {
        Box::pin(async { Ok(BannerInfo::default()) })
    }

    /// One effective configuration value of `profile`.
    fn config_get<'a>(&'a self, profile: &'a str, key: &'a str) -> HostFuture<'a, ConfigValue>;

    /// Closes a session no client is attached to, unless a turn runs or an
    /// input is queued; `true` when it was closed (or was not loaded).
    fn close_idle<'a>(&'a self, profile: &'a str, session_id: &'a str) -> HostFuture<'a, bool>;

    /// Unloads a session no client is attached to without ending it (it
    /// is resumed from storage when reopened), unless a turn runs or an
    /// input is queued; `true` when it was unloaded (or was not loaded).
    fn unload_idle<'a>(&'a self, _profile: &'a str, _session_id: &'a str) -> HostFuture<'a, bool> {
        Box::pin(async { Ok(false) })
    }

    /// Replaces `profile`'s live plugin set (`prometheus plugins reload
    /// [<id>]`, spec: plugin-system, Hot reload respects the prompt-cache
    /// invariant); answers what is active now and what waits for the next
    /// session.
    fn reload_plugins<'a>(
        &'a self,
        _profile: &'a str,
        _plugin: Option<&'a str>,
    ) -> HostFuture<'a, String> {
        Box::pin(async {
            Err(Error::new(
                &pmt_core::error::codes::RPC_INVALID_REQUEST,
                "this daemon hosts no plugins",
            ))
        })
    }

    /// Each served profile's configuration state, for `daemon status`.
    fn config(&self) -> Vec<pmt_rpc::control::ProfileConfig> {
        Vec::new()
    }

    /// Sessions loaded and turns running, for `daemon status`.
    fn load(&self) -> (usize, usize);

    /// Waits until no turn runs, at most `deadline` (a restart drains to a
    /// turn boundary).
    fn drain(&self, deadline: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}

/// Who submitted an input, stamped by the gateway from the connection
/// (spec: tui, Turn authors are stamped by the gateway).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Author {
    /// The client kind from `rpc.hello` (`cli`, `tui`, ...), or `gateway`
    /// for a messaging platform user.
    pub client_kind: String,
    /// The client's id within the daemon.
    pub client_id: String,
    /// Whose authority it acts with: the profile its ticket grants.
    pub principal: String,
    /// When a messaging user sent it (RFC 3339), kept with the message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sent_at: Option<String>,
    /// The send time the model sees in front of the text, in a session
    /// created with message timestamps; never stored in the text.
    #[serde(skip)]
    pub prefix: Option<String>,
}

/// Receives a turn's answer text as it streams.
pub type Deltas = Arc<dyn Fn(&str) + Send + Sync>;

/// A session's way to its attached clients.
#[derive(Clone)]
pub struct Link {
    hub: Arc<Hub>,
    session_id: String,
    request_timeout: Duration,
    approval_timeout: Duration,
    /// Streamed answer text (a messaging conversation that streams).
    deltas: Option<Deltas>,
}

impl std::fmt::Debug for Link {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Link")
            .field("session_id", &self.session_id)
            .field("streams", &self.deltas.is_some())
            .finish_non_exhaustive()
    }
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
            deltas: None,
        }
    }

    /// This link with `deltas` receiving the answer text a turn streams.
    #[must_use]
    pub fn with_deltas(mut self, deltas: Deltas) -> Self {
        self.deltas = Some(deltas);
        self
    }

    /// Whether someone receives streamed answer text.
    #[must_use]
    pub fn streams(&self) -> bool {
        self.deltas.is_some()
    }

    /// Streamed answer text (dropped when nobody streams).
    pub fn delta(&self, text: &str) {
        if let Some(deltas) = &self.deltas {
            deltas(text);
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

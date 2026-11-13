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

fn not_served(why: &str) -> Error {
    Error::new(&pmt_core::error::codes::RPC_NOT_SERVED, why)
}

/// A `queue.*` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueOp {
    /// `queue.list`.
    List(String),
    /// `queue.edit`.
    Edit {
        /// The session.
        session_id: String,
        /// The queued input.
        id: String,
        /// Its new text.
        text: String,
    },
    /// `queue.delete` (the lost turn too).
    Delete {
        /// The session.
        session_id: String,
        /// The queued input.
        id: String,
    },
    /// `queue.pause`.
    Pause(String),
    /// `queue.resume`.
    Resume(String),
}

impl QueueOp {
    /// The session it addresses.
    #[must_use]
    pub fn session_id(&self) -> &str {
        match self {
            Self::List(s) | Self::Pause(s) | Self::Resume(s) => s,
            Self::Edit { session_id, .. } | Self::Delete { session_id, .. } => session_id,
        }
    }
}

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

    /// Undoes a session's file changes (`files.undo`), loading the
    /// session first. Hosts without file snapshots refuse.
    fn files_undo<'a>(
        &'a self,
        profile: &'a str,
        params: pmt_rpc::contracts::FilesUndoParams,
        link: Link,
    ) -> HostFuture<'a, pmt_rpc::contracts::FilesUndone> {
        let _ = (profile, params, link);
        Box::pin(async {
            Err(Error::new(
                &pmt_core::error::codes::RPC_NOT_SERVED,
                "this host has no file snapshots",
            ))
        })
    }

    /// Runs a `worktree.*` request on a session, loading it first. Hosts
    /// without sessions in git checkouts refuse.
    fn worktree<'a>(
        &'a self,
        profile: &'a str,
        request: crate::worktree::WorktreeCall,
        link: Link,
    ) -> HostFuture<'a, crate::worktree::WorktreeReply> {
        let _ = (profile, request, link);
        Box::pin(async {
            Err(Error::new(
                &pmt_core::error::codes::RPC_NOT_SERVED,
                "this host has no worktrees",
            ))
        })
    }

    /// Lists, previews or restores a session workspace's checkpoints
    /// (`files.rollback`). Hosts without file snapshots refuse.
    fn files_rollback<'a>(
        &'a self,
        profile: &'a str,
        params: pmt_rpc::contracts::FilesRollbackParams,
        link: Link,
    ) -> HostFuture<'a, pmt_rpc::contracts::FilesRolledBack> {
        let _ = (profile, params, link);
        Box::pin(async {
            Err(Error::new(
                &pmt_core::error::codes::RPC_NOT_SERVED,
                "this host has no file snapshots",
            ))
        })
    }

    /// One `local.*` method for `profile` (spec: local-models, Local
    /// models command). Hosts without a home on disk do not serve them.
    ///
    /// # Errors
    ///
    /// `rpc.not_served`; the matching `prometheus local` command's errors.
    fn local<'a>(
        &'a self,
        profile: &'a str,
        request: crate::local::LocalRequest,
    ) -> HostFuture<'a, pmt_rpc::contracts::LocalReply> {
        let _ = (profile, request);
        Box::pin(async {
            Err(Error::new(
                &pmt_core::error::codes::RPC_NOT_SERVED,
                "this host has no local models runtime",
            ))
        })
    }

    /// The Mixture of Agents presets of `profile` after `change` (`None`
    /// only reads): `moa.presets.*`. Hosts without profiles on disk
    /// refuse.
    ///
    /// # Errors
    ///
    /// `rpc.not_served`; the change's refusal (`config.invalid`,
    /// `moa.default_preset_protected`, `moa.preset_not_found`).
    fn moa_presets<'a>(
        &'a self,
        profile: &'a str,
        change: Option<crate::moa::Change>,
    ) -> HostFuture<'a, pmt_rpc::contracts::MoaPresetList> {
        let _ = (profile, change);
        Box::pin(async {
            Err(Error::new(
                &pmt_core::error::codes::RPC_NOT_SERVED,
                "this host has no Mixture of Agents presets",
            ))
        })
    }

    /// The credential vault of `profile` and its scope (`vault.*`). Hosts
    /// without profiles on disk refuse.
    ///
    /// # Errors
    ///
    /// `rpc.not_served`; profile errors.
    fn vault(&self, profile: &str) -> Result<crate::vault::VaultAccess, Error> {
        let _ = profile;
        Err(Error::new(
            &pmt_core::error::codes::RPC_NOT_SERVED,
            "this host has no credential vault",
        ))
    }

    /// The running language servers of `profile` (`lsp.status`). Hosts
    /// without profiles on disk refuse.
    ///
    /// # Errors
    ///
    /// `rpc.not_served`; profile errors.
    fn lsp_status(&self, profile: &str) -> Result<pmt_rpc::contracts::LspStatus, Error> {
        let _ = profile;
        Err(Error::new(
            &pmt_core::error::codes::RPC_NOT_SERVED,
            "this host runs no language servers",
        ))
    }

    /// The one-line summary of the agent's memory changes in a session
    /// (`memory.summary`), when there is one to show. Hosts without a
    /// memory journal have none.
    ///
    /// # Errors
    ///
    /// Profile and store errors.
    fn memory_summary(&self, profile: &str, session_id: &str) -> Result<Option<String>, Error> {
        let _ = (profile, session_id);
        Ok(None)
    }

    /// The low-disk prune question for an interactive client's startup
    /// (`autoprune.offer`), when it is due; asking counts as the day's
    /// check. Hosts without a store on disk never ask.
    ///
    /// # Errors
    ///
    /// Profile and store errors.
    fn autoprune_offer(&self, profile: &str) -> Result<Option<String>, Error> {
        let _ = profile;
        Ok(None)
    }

    /// The learning journey of `profile` (`journey.list`), nodes created
    /// at or after `since` when given.
    ///
    /// # Errors
    ///
    /// `rpc.not_served` on hosts without a store; `cli.usage` for a
    /// `since` that does not read; profile and store errors.
    fn journey(
        &self,
        profile: &str,
        since: Option<&str>,
    ) -> Result<pmt_rpc::contracts::JourneyList, Error> {
        let _ = (profile, since);
        Err(Error::new(
            &pmt_core::error::codes::RPC_NOT_SERVED,
            "this host keeps no learning journey",
        ))
    }

    /// The automation blueprints of `profile` (`cron.blueprints.list`).
    ///
    /// # Errors
    ///
    /// `rpc.not_served`; profile errors.
    fn cron_blueprints(&self, profile: &str) -> Result<pmt_rpc::contracts::BlueprintList, Error> {
        let _ = profile;
        Err(Error::new(
            &pmt_core::error::codes::RPC_NOT_SERVED,
            "this host has no scheduled jobs",
        ))
    }

    /// Fills a blueprint of `profile` (`cron.blueprints.fill`).
    ///
    /// # Errors
    ///
    /// `rpc.not_served`; `cron.blueprint` for a bad value; the errors of
    /// creating a job.
    fn cron_blueprint_fill(
        &self,
        profile: &str,
        params: &pmt_rpc::contracts::BlueprintFillParams,
    ) -> Result<pmt_rpc::contracts::BlueprintFilled, Error> {
        let _ = (profile, params);
        Err(Error::new(
            &pmt_core::error::codes::RPC_NOT_SERVED,
            "this host has no scheduled jobs",
        ))
    }

    /// Stops the session's running turn at its next safe point; a no-op
    /// when none runs.
    fn interrupt<'a>(&'a self, profile: &'a str, session_id: &'a str) -> HostFuture<'a, ()>;

    /// Steers the session's running turn (spec: agent-turn-loop,
    /// Structured steering); with no turn running the steer opens the next
    /// user message.
    fn steer_session<'a>(
        &'a self,
        profile: &'a str,
        params: pmt_rpc::contracts::SteerParams,
    ) -> HostFuture<'a, ()> {
        let _ = (profile, params);
        Box::pin(async {
            Err(Error::new(
                &pmt_core::error::codes::RPC_NOT_SERVED,
                "this host cannot steer sessions",
            ))
        })
    }

    /// The session a chat's `-r`/`-c` reference names.
    fn resolve<'a>(&'a self, profile: &'a str, params: ResolveParams)
    -> HostFuture<'a, SessionRef>;

    /// Forks a session (the parent is unchanged).
    fn fork<'a>(&'a self, profile: &'a str, params: ForkParams) -> HostFuture<'a, SessionRef>;

    /// The descendants (`siblings = false`) or the siblings of a session
    /// (`session.tree` / `session.siblings`).
    fn lineage<'a>(
        &'a self,
        _profile: &'a str,
        _session_id: &'a str,
        _siblings: bool,
    ) -> HostFuture<'a, pmt_rpc::contracts::Lineage> {
        Box::pin(async {
            Err(Error::new(
                &pmt_core::error::codes::RPC_NOT_SERVED,
                "this host does not serve session lineage",
            ))
        })
    }

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

    /// The models the session (or, without one, a new chat of
    /// `profile`) may use.
    fn model_options<'a>(
        &'a self,
        profile: &'a str,
        session_id: Option<&'a str>,
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

    /// Applies a validated configuration patch to `profile`
    /// (`config.patch`): all changes or none.
    fn config_patch<'a>(
        &'a self,
        _profile: &'a str,
        _changes: Vec<pmt_rpc::contracts::ConfigChange>,
    ) -> HostFuture<'a, ()> {
        Box::pin(async { Err(not_served("this host has no configuration files")) })
    }

    /// Re-reads `profile`'s configuration now (`config.reload`).
    fn config_reload<'a>(
        &'a self,
        _profile: &'a str,
    ) -> HostFuture<'a, pmt_rpc::contracts::ConfigReloaded> {
        Box::pin(async { Err(not_served("this host has no configuration files")) })
    }

    /// One `queue.*` call on a session of `profile` (spec: desktop-app,
    /// Composer history and queue editing); the session's clients learn
    /// of a change through `link` (`queue.changed`).
    fn queue<'a>(
        &'a self,
        _profile: &'a str,
        _op: QueueOp,
        _link: Link,
    ) -> HostFuture<'a, pmt_rpc::contracts::QueueState> {
        Box::pin(async { Err(not_served("this host keeps no prompt queues")) })
    }

    /// One directory of a session's workspace (`fs.list`), through the
    /// session's own terminal backend.
    fn fs_list<'a>(
        &'a self,
        _profile: &'a str,
        _params: pmt_rpc::contracts::FsListParams,
        _link: Link,
    ) -> HostFuture<'a, pmt_rpc::contracts::FsListing> {
        Box::pin(async { Err(not_served("this host has no session workspaces")) })
    }

    /// The start of one file of a session (`fs.read`), through the
    /// session's own terminal backend.
    fn fs_read<'a>(
        &'a self,
        _profile: &'a str,
        _params: pmt_rpc::contracts::FsReadParams,
        _link: Link,
    ) -> HostFuture<'a, pmt_rpc::contracts::FsFile> {
        Box::pin(async { Err(not_served("this host has no session workspaces")) })
    }

    /// The images, files and links `profile`'s sessions produced
    /// (`artifacts.list`).
    fn artifacts<'a>(
        &'a self,
        _profile: &'a str,
        _params: pmt_rpc::contracts::ArtifactsListParams,
    ) -> HostFuture<'a, pmt_rpc::contracts::ArtifactList> {
        Box::pin(async { Err(not_served("this host keeps no transcripts")) })
    }

    /// `profile`'s canonical Bot Chat, created when it has none
    /// (`bot.chat.ensure`); `true` when created now.
    fn bot_chat<'a>(&'a self, _profile: &'a str) -> HostFuture<'a, (String, bool)> {
        Box::pin(async { Err(not_served("this host has no bot chats")) })
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
    /// What started an automated turn (`goal`, `loop`, `heartbeat`): the
    /// host stores it as the user message's `origin`.
    #[serde(skip)]
    pub origin: Option<String>,
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

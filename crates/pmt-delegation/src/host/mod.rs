//! The delegation host: one session's `delegate_task` over real or fake
//! child sessions (spec: subagent-delegation).
//!
//! A [`Delegator`] is built once per session from its frozen
//! [`SessionContext`] (delegation config, agent definitions, router, the
//! parent's grant), so settings changed mid-session never alter the
//! running session's tool schema. The binary supplies a [`Launcher`] that
//! opens child sessions and, where the surface can take later
//! completions, a [`Completions`] sink; everything else (validation,
//! grants, budgets, queueing, stall detection, progress, results, control)
//! happens here.

mod child;
mod control;
pub mod deliver;
pub mod plugin;
mod review;
mod spawn;
mod tracker;
mod units;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use pmt_agent::turn::model::BoxFuture;
use pmt_agent::turn::{EventSink, Interrupt, TurnUsage};
use pmt_config::ProfileScope;
use pmt_config::schema::DelegationConfig;
use pmt_core::cancel::CancelToken;
use pmt_core::error::{Error, codes};
use pmt_core::id::SessionId;
use pmt_store::Store;
use pmt_tools::{ToolRegistry, ToolResult};
use serde_json::Value;

use crate::Refusal;
use crate::agents::Definitions;
use crate::budget::{ChildBudget, OneShotBudget};
use crate::contract::{self, Action};
use crate::grant::{Grant, ParentGrant};
use crate::router::Router;
use crate::routing::ModelChoice;

pub(crate) use control::steer_entry;
pub use review::Labelled;
pub use tracker::Tracker;

/// How a child's turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnEnd {
    /// A final answer.
    FinalAnswer,
    /// The iteration budget ran out.
    MaxIterations,
    /// A token or cost budget ran out.
    BudgetExhausted,
    /// It was stopped.
    Interrupted,
    /// It failed.
    Error,
}

/// One turn of a child.
#[derive(Debug, Clone)]
pub struct TurnOutcome {
    /// The answer text.
    pub text: Option<String>,
    /// How it ended.
    pub end: TurnEnd,
    /// Model calls made.
    pub iterations: u32,
    /// Usage of the turn.
    pub usage: TurnUsage,
    /// The failure, when `end` is `Error`: code and message.
    pub error: Option<(String, String)>,
}

/// What closing a child left behind.
#[derive(Debug, Clone, Default)]
pub struct Closed {
    /// Processes that did not stop within 10 seconds.
    pub orphaned_processes: Vec<String>,
    /// The child's own git worktree and what became of it.
    pub worktree: Option<crate::result::WorktreeReport>,
    /// Why the child ran without the isolation it was configured for.
    pub note: Option<String>,
}

/// A running child session.
pub trait ChildSession: Send + Sync {
    /// Its session id.
    fn id(&self) -> SessionId;
    /// The `provider/model` it runs on.
    fn model(&self) -> String;
    /// Its working directory (relative artifact paths resolve there).
    fn cwd(&self) -> Option<PathBuf> {
        None
    }
    /// Runs one turn on `input` under `interrupt`.
    fn turn(&self, input: String, interrupt: Interrupt) -> BoxFuture<'_, TurnOutcome>;
    /// Queues a steer for its next iteration; `false` when it cannot take
    /// one.
    fn steer(&self, text: &str) -> bool;
    /// Steers it never used.
    fn undelivered_steers(&self) -> Vec<String>;
    /// Ends it: stops its background processes and kernels (except those
    /// handed off to the parent) and closes its session.
    fn close(&self) -> BoxFuture<'_, Closed>;
}

/// How a child's approvals are resolved.
#[derive(Debug, Clone)]
pub struct ApprovalPlan {
    /// `delegation.child_approval`.
    pub policy: pmt_config::schema::ChildApproval,
    /// How long an escalation waits.
    pub timeout: Duration,
}

/// Everything a launcher needs to open one child.
#[derive(Clone)]
pub struct ChildPlan {
    /// `sa-<ULID>`.
    pub subagent_id: String,
    /// The spawning session.
    pub parent: SessionId,
    /// Depth from 1.
    pub depth: u32,
    /// Its grant.
    pub grant: Grant,
    /// Its model.
    pub model: ModelChoice,
    /// A profile route.
    pub profile: Option<String>,
    /// A remote backend a router chose.
    pub backend: Option<String>,
    /// Its ephemeral system prompt.
    pub system: String,
    /// The parent's frozen memory snapshot (read-only).
    pub memory: Option<String>,
    /// Its limits.
    pub budget: ChildBudget,
    /// Its approvals.
    pub approvals: ApprovalPlan,
    /// Image references of its task.
    pub images: Vec<String>,
    /// Where its trajectory events also go.
    pub events: Arc<dyn EventSink>,
}

impl std::fmt::Debug for ChildPlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChildPlan")
            .field("subagent_id", &self.subagent_id)
            .field("depth", &self.depth)
            .field("grant", &self.grant)
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

/// Opens child sessions (the binary implements it with real sessions).
pub trait Launcher: Send + Sync {
    /// Opens the child `plan` describes.
    ///
    /// # Errors
    ///
    /// The child could not be built (a pinned model that fails route
    /// validation, a missing profile, a missing credential...).
    fn open<'a>(
        &'a self,
        plan: &'a ChildPlan,
    ) -> BoxFuture<'a, Result<Arc<dyn ChildSession>, Error>>;
}

/// Delivers a background delegation's completion to its session as a
/// `delegation_completed` input.
pub trait Completions: Send + Sync {
    /// Hands `text` to the owning session; `false` when it could not take
    /// it now (it is retried later).
    fn deliver<'a>(&'a self, delegation_id: &'a str, text: String) -> BoxFuture<'a, bool>;
}

/// How spawns of this session run.
#[derive(Clone)]
pub enum Dispatch {
    /// In the background where allowed; completions go to the sink.
    Background(Arc<dyn Completions>),
    /// Always synchronously, with the reason given in the result's note.
    Synchronous(String),
}

/// What one session's delegation is built from, frozen at session start.
pub struct SessionContext {
    /// The profile.
    pub scope: ProfileScope,
    /// The profile's store.
    pub store: Arc<Store>,
    /// The session.
    pub session: SessionId,
    /// Its root lineage.
    pub root: SessionId,
    /// Depth of the session (0: a top-level session).
    pub depth: u32,
    /// `[delegation]` as the session started.
    pub config: DelegationConfig,
    /// The tool registry (toolset expansion).
    pub registry: Arc<ToolRegistry>,
    /// Agent definitions as the session started.
    pub agents: Definitions,
    /// The session's router, or why it is unavailable.
    pub router: Result<Arc<dyn Router>, Refusal>,
    /// The parent's frozen memory snapshot.
    pub memory: Option<String>,
    /// How spawns run.
    pub dispatch: Dispatch,
    /// The spawn budget of a finite one-shot run.
    pub oneshot: Option<Arc<OneShotBudget>>,
    /// The root session's event stream.
    pub events: Arc<dyn EventSink>,
    /// Opens children.
    pub launcher: Arc<dyn Launcher>,
    /// Where full summaries are kept (readable by the session).
    pub spill_dir: PathBuf,
    /// Whether spawning is paused for the profile (`/delegation pause`).
    pub paused: Arc<dyn Fn() -> bool + Send + Sync>,
    /// Tokens left in the parent's context window, when known.
    pub context_left: Arc<dyn Fn() -> Option<u64> + Send + Sync>,
    /// The session's form of `delegate_task` (joining, groups).
    pub shape: crate::shape::Shape,
}

/// One session's delegation.
pub struct Delegator {
    ctx: Arc<SessionContext>,
    /// The session's router (frozen at start; `/delegation router --now`
    /// replaces it).
    router: Mutex<Result<Arc<dyn Router>, Refusal>>,
    parent: Mutex<ParentGrant>,
    closed: AtomicBool,
    /// The process is stopping: children are left to the next start's
    /// recovery and completions stay owed in the store.
    suspended: AtomicBool,
    /// The session's background children run under it: `/stop all`
    /// cancels it (and starts a new one), closing the session too.
    lifetime: Mutex<CancelToken>,
}

impl Delegator {
    /// The delegation of the session `ctx` describes.
    #[must_use]
    pub fn new(ctx: SessionContext) -> Arc<Self> {
        Arc::new(Self {
            router: Mutex::new(ctx.router.clone()),
            ctx: Arc::new(ctx),
            parent: Mutex::default(),
            closed: AtomicBool::new(false),
            suspended: AtomicBool::new(false),
            lifetime: Mutex::new(CancelToken::new()),
        })
    }

    /// The session's context.
    #[must_use]
    pub fn context(&self) -> &SessionContext {
        &self.ctx
    }

    /// Sets what the session holds (its surface's tools and its
    /// capabilities), known once its tools are built.
    pub fn set_parent(&self, parent: ParentGrant) {
        *self.parent.lock().unwrap_or_else(PoisonError::into_inner) = parent;
    }

    fn parent(&self) -> ParentGrant {
        self.parent
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The conversation key of the session (control actions are scoped to
    /// its spawn tree).
    #[must_use]
    pub fn conversation(&self) -> String {
        conversation_of(&self.ctx.store, self.ctx.session)
    }

    /// Runs one `delegate_task` call made in a turn whose hard stop is
    /// `cancel` (synchronous children stop with it).
    pub async fn call(self: &Arc<Self>, arguments: &Value, cancel: &CancelToken) -> ToolResult {
        let max = usize::try_from(self.ctx.config.max_tasks_per_call).unwrap_or(8);
        let params = self.router_params();
        let parsed = match contract::parse(arguments, max, &params) {
            Ok(call) => call,
            Err(refusal) => return refusal.tool_result(),
        };
        let outcome = match parsed.action {
            Action::Spawn => self.spawn(parsed, cancel).await,
            other if self.ctx.shape.joining => Err(Refusal::new(
                &codes::DELEGATION_INVALID_TASK,
                format!(
                    "action={} is not available: this session waits for its children inside the call.",
                    other.as_str()
                ),
            )),
            _ => self.control(&parsed).await,
        };
        outcome.unwrap_or_else(|refusal| refusal.tool_result())
    }

    fn router_params(&self) -> Vec<String> {
        self.router()
            .map(|r| r.params().keys().cloned().collect())
            .unwrap_or_default()
    }

    /// The session's router, or why it is unavailable.
    ///
    /// # Errors
    ///
    /// `delegation.router_unavailable`.
    pub fn router(&self) -> Result<Arc<dyn Router>, Refusal> {
        self.router
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Replaces the session's router (`/delegation router <name> --now`).
    pub fn set_router(&self, router: Result<Arc<dyn Router>, Refusal>) {
        *self.router.lock().unwrap_or_else(PoisonError::into_inner) = router;
    }

    /// Ends the session's delegation (`/stop all`, `/new`, close): every
    /// child of the session is stopped and later spawns fail with
    /// `delegation.session_closed`.
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        if !self.suspended() {
            self.stop_all();
        }
    }

    /// The process is stopping or restarting (not the session closing):
    /// no further spawn, running background children are not stopped
    /// (they end with the process and the next start finalises them as
    /// `interrupted`), and nothing is delivered now; every completion
    /// stays owed in the store and reaches the session when it is next
    /// loaded.
    pub fn suspend(&self) {
        self.suspended.store(true, Ordering::SeqCst);
        self.closed.store(true, Ordering::SeqCst);
    }

    fn suspended(&self) -> bool {
        self.suspended.load(Ordering::SeqCst)
    }

    /// Stops every open child of the session (`/stop all`) without closing
    /// it.
    pub fn stop_all(&self) {
        let previous = std::mem::replace(
            &mut *self.lifetime.lock().unwrap_or_else(PoisonError::into_inner),
            CancelToken::new(),
        );
        previous.cancel();
        for entry in crate::tree::Registry::process().open_children_of(&self.conversation()) {
            entry.request_stop(pmt_agent::turn::Issuer::User);
        }
    }

    /// The token background children of the session run under now.
    fn lifetime(&self) -> CancelToken {
        self.lifetime
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn check_open(&self) -> Result<(), Refusal> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(Refusal::new(
                &codes::DELEGATION_SESSION_CLOSED,
                "This session is closed; it cannot spawn subagents.",
            ));
        }
        Ok(())
    }
}

/// The conversation a session belongs to: the session its compression
/// continuations descend from.
#[must_use]
pub fn conversation_of(store: &Store, session: SessionId) -> String {
    let mut current = session;
    for _ in 0..64 {
        let Ok(row) = store.session(current) else {
            break;
        };
        match (row.parent_session_id, row.lineage_kind) {
            (Some(parent), Some(pmt_store::LineageKind::Compression)) => current = parent,
            _ => break,
        }
    }
    current.to_string()
}

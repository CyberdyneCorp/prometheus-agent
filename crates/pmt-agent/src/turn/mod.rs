//! The agent turn loop (spec: agent-turn-loop).
//!
//! Every surface runs a turn through [`Agent::run_turn`]: admission under
//! the session's turn lease, the frozen system prompt, then iterations of
//! interrupt and budget checks, steer delivery, request assembly from the
//! repaired transcript, the cache-prefix check, the model call (with
//! auto-recovery), response intake (empty-response and narration guards)
//! and either a tool round (planned batch, guardrails) or the final answer,
//! and a finalizer that runs the budget summary handoff, writes the
//! failed-turn boundary row and emits `turn.end`. A turn always ends with
//! exactly one [`ExitReason`].

pub mod breakpoints;
mod budget;
mod cache;
mod compression;
pub mod events;
mod exit;
mod finish;
mod guardrails;
mod guards;
pub mod interrupt;
pub mod iteration;
pub mod lease;
pub mod model;
mod recovery;
pub mod repair;
mod round;
mod run;
mod steer;
mod system;
pub mod tools;
pub mod transcript;

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use jiff::Timestamp;
use pmt_config::ProfileScope;
use pmt_core::cancel::CancelToken;
use pmt_core::error::{Error, codes};
use pmt_core::id::SessionId;
use pmt_core::secret::Redactor;
use pmt_obs::budget::Budgets;
use pmt_providers::request::CacheTtl;
use pmt_store::Store;

pub use compression::CONTEXT_FULL;
pub use events::{EventSink, NoEvents, Recorded};
pub use exit::{ExitReason, TurnId, TurnResult, TurnUsage};
pub use interrupt::{Interrupt, Issuer};
pub use iteration::IterationBudget;
pub use lease::LeaseTiming;
pub use model::{DeltaSink, ModelPort, NoDeltas, ProviderModel, ProviderSetup};
pub use round::WRAP_UP_NOTICE;
pub use tools::{SessionTools, Timings, ToolBatch, ToolPort};

use crate::compress::Summarizer;
use crate::prompt::frozen::Rebuild;
use crate::references::{Places, UrlSource};

/// The user-role notice of the budget summary handoff.
pub const HANDOFF_NOTICE: &str = "You have reached this turn's budget. Give a final response summarizing what you found and did so far, without calling tools.";

/// What the session's prompt is built from, beyond the profile config.
#[derive(Debug, Clone)]
pub struct PromptSetup {
    /// The surface (`cli`, `telegram`...).
    pub platform: String,
    /// An adapter-registered platform hint.
    pub registered_hint: Option<String>,
    /// The session's working directory.
    pub cwd: Option<PathBuf>,
    /// `--no-context-files`.
    pub no_context_files: bool,
    /// `--pass-session-id`.
    pub pass_session_id: bool,
    /// The caller's system message.
    pub caller_system: Option<String>,
    /// Skills index text (skills subsystem).
    pub skills_index: Option<String>,
    /// Pinned skills text (skills subsystem).
    pub pinned_skills: Option<String>,
    /// Memory snapshot (memory subsystem).
    pub memory: Option<String>,
    /// Plugin sections.
    pub plugin_sections: Vec<String>,
    /// When the session started.
    pub started_at: Timestamp,
}

impl PromptSetup {
    /// A setup for `platform` in `cwd` with nothing else.
    #[must_use]
    pub fn new(platform: &str, cwd: Option<PathBuf>) -> Self {
        Self {
            platform: platform.to_owned(),
            registered_hint: None,
            cwd,
            no_context_files: false,
            pass_session_id: false,
            caller_system: None,
            skills_index: None,
            pinned_skills: None,
            memory: None,
            plugin_sections: Vec::new(),
            started_at: Timestamp::now(),
        }
    }
}

/// `@` reference expansion of a surface that offers it (CLI, TUI,
/// desktop, ACP); messaging adapters leave it unset.
pub struct ReferenceSetup {
    /// Paths the references may reach.
    pub places: Places,
    /// `@url:` fetching.
    pub urls: Option<Arc<dyn UrlSource>>,
}

/// Whether `platform` is an interactive surface (a person at a terminal
/// or desktop app); gateway, cron, API and kanban sessions are not.
#[must_use]
pub fn interactive_platform(platform: &str) -> bool {
    matches!(platform, "cli" | "tui" | "desktop" | "acp")
}

/// One session's agent: its ports and settings.
pub struct AgentSetup {
    /// The profile the session belongs to.
    pub scope: ProfileScope,
    /// The profile's store.
    pub store: Arc<Store>,
    /// The session.
    pub session: SessionId,
    /// The model.
    pub model: Arc<dyn ModelPort>,
    /// The tools.
    pub tools: Arc<dyn ToolPort>,
    /// Trajectory events.
    pub events: Arc<dyn EventSink>,
    /// Prompt inputs.
    pub prompt: PromptSetup,
    /// `@` reference expansion, when the surface offers it.
    pub references: Option<ReferenceSetup>,
    /// An interactive surface (CLI, TUI, desktop, ACP): tool loop
    /// guardrails only warn there unless `hard_stop_enabled`.
    pub interactive: bool,
    /// Cron, unattended gateway, background delegation, evolution.
    pub unattended: bool,
    /// Redacts secrets from the failed-turn boundary row.
    pub redactor: Redactor,
    /// This agent's own iteration budget (a delegated child's); `None`
    /// reads `agent.max_iterations`.
    pub max_iterations: Option<u32>,
    /// The auxiliary `compression` route; `None` compacts with the
    /// deterministic fallback summary.
    pub summarizer: Option<Arc<dyn Summarizer>>,
    /// Re-reads the prompt inputs that live on disk (memory snapshot,
    /// skills index, plugin sections) before every prompt rebuild, so a
    /// compaction or `--now` rebuild sees the current state.
    pub refresh_prompt: Option<RefreshPrompt>,
}

/// Updates a [`PromptSetup`] from the current disk state.
pub type RefreshPrompt = Arc<dyn Fn(&mut PromptSetup) + Send + Sync>;

impl AgentSetup {
    /// A setup with no events and no references, attended, interactive as
    /// its platform is.
    #[must_use]
    pub fn new(
        scope: ProfileScope,
        store: Arc<Store>,
        session: SessionId,
        model: Arc<dyn ModelPort>,
        tools: Arc<dyn ToolPort>,
        prompt: PromptSetup,
    ) -> Self {
        let interactive = interactive_platform(&prompt.platform);
        Self {
            scope,
            store,
            session,
            model,
            tools,
            events: Arc::new(NoEvents),
            prompt,
            references: None,
            interactive,
            unattended: false,
            redactor: Redactor::default(),
            max_iterations: None,
            summarizer: None,
            refresh_prompt: None,
        }
    }
}

/// Per-call options of [`Agent::run_turn`].
pub struct TurnOptions {
    /// Stops the turn (clone it before passing the options).
    pub interrupt: Interrupt,
    /// `--run-budget`: overrides `agent.run_budget_secs`.
    pub run_budget: Option<Duration>,
    /// Lease lifetimes.
    pub lease: LeaseTiming,
    /// Streamed answer text.
    pub deltas: Arc<dyn DeltaSink>,
}

impl TurnOptions {
    /// Default options under `parent` (session shutdown cancels the turn).
    #[must_use]
    pub fn new(parent: &CancelToken) -> Self {
        Self {
            interrupt: Interrupt::child_of(parent),
            run_budget: None,
            lease: LeaseTiming::default(),
            deltas: Arc::new(NoDeltas),
        }
    }
}

impl Default for TurnOptions {
    fn default() -> Self {
        Self::new(&CancelToken::new())
    }
}

/// What the user sent.
#[derive(Debug, Clone, Default)]
pub struct TurnInput {
    /// The message text.
    pub text: String,
    /// Request-time additions (memory prefetch, plugin context): sent in
    /// this turn's user message only, never in the system prompt.
    pub additions: Vec<String>,
}

impl TurnInput {
    /// A plain message.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            additions: Vec::new(),
        }
    }
}

/// One session's agent.
pub struct Agent {
    setup: AgentSetup,
    budgets: Budgets,
    pending_rebuild: Mutex<Option<Rebuild>>,
    personality: Mutex<Option<String>>,
    steers: steer::Steers,
    /// The previous request's prefix (cache break detection).
    prefix: Mutex<Option<cache::Prefix>>,
    /// Reported usage, for the compression preflight.
    usage: Mutex<compression::UsageMemory>,
    /// The prompt-cache lifetime, resolved once per session.
    cache_ttl: OnceLock<CacheTtl>,
    /// The TTL lowering was logged.
    ttl_lowered: std::sync::atomic::AtomicBool,
}

impl Agent {
    /// The agent of `setup`.
    ///
    /// # Errors
    ///
    /// `config.timezone` for an unknown `cron.timezone` (budget periods).
    pub fn new(setup: AgentSetup) -> Result<Self, Error> {
        let zone = pmt_obs::time::profile_zone(&setup.scope.config().cron)?;
        Ok(Self {
            setup,
            budgets: Budgets::new(zone),
            pending_rebuild: Mutex::new(None),
            personality: Mutex::new(None),
            steers: steer::Steers::default(),
            prefix: Mutex::new(None),
            usage: Mutex::new(compression::UsageMemory::new()),
            cache_ttl: OnceLock::new(),
            ttl_lowered: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// The session this agent runs.
    #[must_use]
    pub fn session(&self) -> SessionId {
        self.setup.session
    }

    /// Rebuilds the system prompt at the next turn (`--now`, compaction),
    /// a recorded cache break.
    pub fn rebuild_prompt(&self, why: Rebuild) {
        *self
            .pending_rebuild
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(why);
    }

    /// `/personality <name> --now`: the next turn rebuilds the prompt
    /// with `name` (`none` clears it) instead of the session's config.
    pub fn apply_personality_now(&self, name: &str) {
        *self
            .personality
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(name.to_owned());
        self.rebuild_prompt(Rebuild::Now("personality".to_owned()));
    }

    /// Steers the running turn of this session (`session.steer`): the
    /// text reaches the model after the next tool results, or with the
    /// next user message when the turn ends first.
    ///
    /// # Errors
    ///
    /// `turn.empty_steer` (`empty steer`) for empty or whitespace text.
    pub fn steer(&self, text: &str) -> Result<(), Error> {
        self.steers.push(text)
    }

    /// Replaces the reference prefix, returning the previous one.
    fn swap_prefix(&self, next: Option<cache::Prefix>) -> Option<cache::Prefix> {
        std::mem::replace(
            &mut *self.prefix.lock().unwrap_or_else(PoisonError::into_inner),
            next,
        )
    }

    /// The session's prompt-cache lifetime (resolved once), lowered to
    /// what `route` keeps.
    fn cache_ttl(
        &self,
        config: &pmt_config::schema::PromptCacheConfig,
        route: &model::RouteInfo,
    ) -> CacheTtl {
        let session = *self
            .cache_ttl
            .get_or_init(|| breakpoints::session_ttl(config.ttl, &self.setup.prompt.platform));
        let kept = breakpoints::supported(session, route.wire_format);
        if kept != session
            && !self
                .ttl_lowered
                .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            tracing::debug!(
                provider = %route.provider,
                "prompt cache TTL lowered to the longest the provider keeps"
            );
        }
        kept
    }

    fn usage_memory(&self) -> MutexGuard<'_, compression::UsageMemory> {
        self.usage.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn personality_override(&self) -> Option<String> {
        self.personality
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn take_rebuild(&self) -> Option<Rebuild> {
        self.pending_rebuild
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

    /// Runs one turn of `session` with `input`.
    ///
    /// # Errors
    ///
    /// `turn.scope_required` without a scope, `turn.scope_mismatch` for
    /// another profile's scope or another session; store failures. Every
    /// other outcome is a [`TurnResult`] with its exit reason.
    pub async fn run_turn(
        &self,
        scope: Option<&ProfileScope>,
        session: SessionId,
        input: TurnInput,
        options: TurnOptions,
    ) -> Result<TurnResult, Error> {
        let Some(scope) = scope else {
            return Err(Error::new(
                &codes::TURN_SCOPE_REQUIRED,
                "profile scope required",
            ));
        };
        if scope.dir() != self.setup.scope.dir() || session != self.setup.session {
            return Err(Error::new(
                &codes::TURN_SCOPE_MISMATCH,
                format!(
                    "this agent runs session {} of profile '{}', not session {session} of '{}'",
                    self.setup.session,
                    self.setup.scope.name(),
                    scope.name()
                ),
            ));
        }
        run::run(self, scope, input, options).await
    }
}

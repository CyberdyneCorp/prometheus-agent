//! The session's tools (spec: tool-system; cli, Deferred effects with
//! `--now`): the registry and toolset selection of the profile, the
//! approval gate, the dispatcher over the frozen surface and a port the
//! session can rebuild at a turn boundary (`/tools ... --now`).

use std::path::Path;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use pmt_agent::tools::AgentTools;
use pmt_agent::turn::model::BoxFuture;
use pmt_agent::turn::model::RequestedCall;
use pmt_agent::turn::{SessionTools, Timings, ToolBatch, ToolPort};
use pmt_config::grants::SessionGrants;
use pmt_config::{Config, ProfileScope};
use pmt_core::cancel::CancelToken;
use pmt_core::capability::CapabilitySet;
use pmt_core::error::Error;
use pmt_core::human::Channel;
use pmt_core::panic::CrashReporter;
use pmt_core::secret::Redactor;
use pmt_providers::request::Tool;
use pmt_store::Store;
use pmt_tools::approval::{ApprovalRequest, CommandGate, Context, Decision, Floors, GateSetup};
use pmt_tools::budget::ResultBudget;
use pmt_tools::dispatch::ToolHooks;
use pmt_tools::probe::ProbeCache;
use pmt_tools::protected::ProtectedFiles;
use pmt_tools::variants::RouteTraits;
use pmt_tools::{
    DispatchSetup, Dispatcher, Selection, SessionSurface, SurfaceInputs, ToolRegistry, ToolSurface,
};

/// The approval gate of a session and, on an interactive surface, the
/// receiving end of its prompts.
pub struct Gate {
    /// The gate.
    pub gate: Arc<CommandGate>,
    /// Prompts to show (interactive sessions only).
    pub prompts: Option<pmt_core::human::Surface<ApprovalRequest, Decision>>,
}

/// What builds a session's gate.
pub struct GateInputs<'a> {
    /// The profile.
    pub scope: &'a ProfileScope,
    /// The session's grants.
    pub grants: &'a Arc<SessionGrants>,
    /// The session's working directory.
    pub cwd: &'a Path,
    /// The local user's home.
    pub user_home: Option<&'a Path>,
    /// Someone can answer prompts.
    pub interactive: bool,
    /// A scheduled run: approvals follow `approvals.cron_mode`.
    pub cron: bool,
    /// `--yolo`.
    pub yolo: bool,
    /// Plugin approval observers and transport.
    pub hooks: Option<Arc<dyn pmt_tools::approval::hooks::ApprovalHooks>>,
    /// The session id (its decisions go to the security audit log).
    pub session_id: String,
    /// The smart-mode approval guardian.
    pub guardian: Option<Arc<dyn pmt_tools::approval::guardian::Guardian>>,
    /// Runs approval `explain` previews.
    pub previewer: Option<pmt_tools::approval::explain::Previewer>,
    /// In a daemon-hosted session: the daemon and the session's own
    /// background processes (the self-termination guard).
    pub hosted: Option<(
        pmt_tools::approval::selfterm::Daemon,
        pmt_tools::approval::OwnProcesses,
    )>,
}

/// The approval gate: prompts go to the person on an interactive
/// surface; a one-shot run decides by `approvals.single_query_mode`.
///
/// # Errors
///
/// `[security]` protected-file patterns that do not parse.
pub fn gate(inputs: &GateInputs<'_>) -> Result<Gate, Error> {
    let config = inputs.scope.config();
    let (channel, prompts, context) = if inputs.interactive {
        let (channel, surface) =
            Channel::attached(Duration::from_secs(config.approvals.timeout_secs.max(1)));
        (channel, Some(surface), Context::Interactive)
    } else if inputs.cron {
        (Channel::unattended(), None, Context::Cron)
    } else {
        (Channel::unattended(), None, Context::SingleQuery)
    };
    Ok(Gate {
        gate: Arc::new(build_gate(
            inputs,
            channel,
            context,
            config.approvals.clone(),
        )?),
        prompts,
    })
}

/// A delegated child's gate (spec: subagent-delegation, Child approvals
/// fail closed): every approval goes to `channel` (the child's forwarder,
/// which denies or escalates), never decided by a policy, a guardian or
/// `--yolo`; `approvals.mode` `off` or `smart` asks all the same.
///
/// # Errors
///
/// As [`gate`].
pub fn child_gate(
    inputs: &GateInputs<'_>,
    channel: Channel<ApprovalRequest, Decision>,
) -> Result<Arc<CommandGate>, Error> {
    let mut approvals = inputs.scope.config().approvals.clone();
    approvals.mode = pmt_config::schema::ApprovalMode::Manual;
    Ok(Arc::new(build_gate(
        inputs,
        channel,
        Context::Interactive,
        approvals,
    )?))
}

fn build_gate(
    inputs: &GateInputs<'_>,
    channel: Channel<ApprovalRequest, Decision>,
    context: Context,
    approvals: pmt_config::schema::ApprovalsConfig,
) -> Result<CommandGate, Error> {
    let config = inputs.scope.config();
    let binary = std::env::current_exe().ok();
    let floors = Floors::new(inputs.cwd)
        .user_home(inputs.user_home)
        .prometheus_home(inputs.scope.home())
        .binary(binary.as_deref());
    let gate = CommandGate::new(GateSetup {
        approvals,
        context,
        floors,
        grants: Arc::clone(inputs.grants),
        protected: ProtectedFiles::from_config(&config.security)?,
        channel,
        yolo: inputs.yolo,
        hooks: inputs.hooks.clone(),
    })
    .with_audit(
        pmt_core::audit::AuditLog::of(inputs.scope.dir()),
        inputs.session_id.clone(),
    );
    let gate = match &inputs.guardian {
        Some(guardian) => gate.with_guardian(Arc::clone(guardian)),
        None => gate,
    };
    let gate = match &inputs.previewer {
        Some(previewer) => gate.with_previewer(previewer.clone()),
        None => gate,
    };
    Ok(match &inputs.hosted {
        Some((daemon, own)) => gate.hosted_by(daemon.clone(), Arc::clone(own)),
        None => gate,
    })
}

/// The tool selection of a session of `surface` (spec: tool-system,
/// Per-surface default toolsets): `toolsets.surfaces.<surface>`, else
/// `tools.enabled_toolsets` with `core` standing for the surface's
/// default, minus the disabled toolsets; a non-interactive run drops the
/// tools that need a person.
///
/// `plugins` are the toolsets of the profile's enabled plugins: enabling a
/// plugin opts its tools in (`tools.disabled_toolsets` still removes them),
/// except on webhook-triggered runs (read-only unless the surface or route
/// names toolsets) and for a toolset named after a messaging platform,
/// which reaches only that platform's sessions through its bundle.
///
/// # Errors
///
/// An unknown toolset (`registry.unknown_name`, exit 78).
pub fn selection(
    registry: &ToolRegistry,
    scope: &ProfileScope,
    interactive: bool,
    plugins: &[String],
    surface: &SessionSurface,
) -> Result<Selection, Error> {
    let config = scope.config();
    let mut enabled = match (surface, &config.api_server.toolsets) {
        (SessionSurface::Api, Some(listed)) => api_toolsets(registry, config, listed),
        _ => surface.starting_toolsets(registry, &config.tools, &config.toolsets),
    };
    crate::tools_cmd::opt_in(&mut enabled, plugins, surface);
    let disabled = &config.tools.disabled_toolsets;
    let hidden = if surface.folded_toolsets().is_empty() {
        Vec::new()
    } else {
        super::desktop_prefs::hidden_tools(scope.home())
    };
    let selection = Selection::resolve(registry, &enabled, disabled)?
        .folded_for(registry, surface, disabled, &hidden)?;
    Ok(if interactive {
        selection
    } else {
        selection.without_interactive(registry)
    })
}

/// `api_server.toolsets`, keeping only toolsets whose tools the profile
/// enables (`tools.enabled_toolsets`, `core` standing for the API
/// surface's own default); the others are dropped with a warning (spec:
/// api-server, Unattended approvals and the API toolset).
fn api_toolsets(registry: &ToolRegistry, config: &Config, listed: &[String]) -> Vec<String> {
    let profile = SessionSurface::Api.starting_toolsets(
        registry,
        &config.tools,
        &pmt_config::schema::ToolsetsConfig::default(),
    );
    let allowed: std::collections::BTreeSet<String> = profile
        .iter()
        .filter_map(|name| registry.members(name).ok())
        .flatten()
        .collect();
    listed
        .iter()
        .filter(|name| {
            let within = registry
                .members(name)
                .is_ok_and(|members| members.iter().all(|m| allowed.contains(m)));
            if !within {
                tracing::warn!(
                    toolset = name.as_str(),
                    "api_server.toolsets names a toolset the profile does not enable; dropped"
                );
            }
            within
        })
        .cloned()
        .collect()
}

/// The surface stored session `session` was started from: a messaging
/// session's platform (`webhook` for webhook-triggered runs), else its
/// source; sources without a surface of their own (subagents, evals,
/// imports) start like the CLI.
#[must_use]
pub fn surface_of(session: &pmt_store::Session) -> SessionSurface {
    use pmt_store::{Origin, Source};
    if let Origin::Gateway { platform, .. } = &session.origin {
        return SessionSurface::Gateway(platform.to_lowercase());
    }
    match session.source {
        Source::Tui => SessionSurface::Tui,
        Source::Desktop if session.source_tag.as_deref() == Some(ONBOARDING_TAG) => {
            SessionSurface::Onboarding
        }
        Source::Desktop => SessionSurface::Desktop,
        Source::Cron => SessionSurface::Cron,
        Source::Api => SessionSurface::Api,
        Source::Acp if session.source_tag.as_deref() == Some(ACP_UNATTENDED_TAG) => {
            SessionSurface::AcpUnattended
        }
        Source::Acp => SessionSurface::Acp,
        _ => SessionSurface::Cli,
    }
}

/// The source tag of unattended ACP sessions (spec: acp-adapter).
const ACP_UNATTENDED_TAG: &str = "acp-unattended";

/// The source tag of the desktop's first-run onboarding chat (spec:
/// desktop-app, First-run onboarding chat).
pub const ONBOARDING_TAG: &str = "onboarding";

/// The schema-variant traits of `model`'s current route.
pub async fn route_traits(model: &dyn pmt_agent::turn::model::ModelPort) -> RouteTraits {
    let route = model.route().await;
    RouteTraits::of(&route.provider, &route.model, route.wire_format)
        .with_context_window(route.context_window)
}

/// [`route_traits`] with the auxiliary capabilities session `id` reaches
/// (`video`: its `vision` route takes video, so `video_analyze` works).
pub async fn session_route(
    model: &dyn pmt_agent::turn::model::ModelPort,
    id: pmt_core::id::SessionId,
) -> RouteTraits {
    let mut traits = route_traits(model).await;
    let video = pmt_media::MediaSessions::process()
        .session(&id.to_string())
        .is_some_and(|media| media.setup().video.is_some());
    if video {
        traits
            .auxiliary
            .insert(pmt_media::video::VIDEO_AUX.to_owned());
    }
    traits
}

/// What one surface build needs.
pub struct SurfaceInputsOwned<'a> {
    /// The profile.
    pub scope: &'a ProfileScope,
    /// The store (the surface is recorded for `tools list --session`).
    pub store: &'a Store,
    /// The session.
    pub session: pmt_core::id::SessionId,
    /// The registry.
    pub registry: &'a ToolRegistry,
    /// The selection.
    pub selection: &'a Selection,
    /// The effective capabilities.
    pub capabilities: &'a CapabilitySet,
    /// The gate.
    pub gate: &'a Arc<CommandGate>,
    /// Agent-level tools (none for a background review).
    pub agent: Option<&'a Arc<AgentTools>>,
    /// Known secrets.
    pub redactor: &'a Redactor,
    /// The route's context window (result budget).
    pub context_window: Option<u64>,
    /// Plugin tool hooks (`pre_tool_call`, `post_tool_call`).
    pub hooks: Arc<dyn ToolHooks>,
    /// The main route schema variants are picked for.
    pub route: &'a RouteTraits,
}

/// Builds the surface and the dispatcher over it, and records the
/// surface for `tools list --session`.
///
/// # Errors
///
/// Store errors recording the surface.
pub async fn session_tools(inputs: &SurfaceInputsOwned<'_>) -> Result<Arc<SessionTools>, Error> {
    let (dispatcher, timings) = dispatcher(inputs).await?;
    Ok(Arc::new(SessionTools::new(
        inputs.agent.map(Arc::clone),
        Arc::new(dispatcher),
        timings,
    )))
}

/// `surface` with the hosted connector tools behind its tool-search bridge
/// (spec: connectors, Remote connector tools through the search bridge),
/// for a turn-loop session whose selection has `manage_connections` and a
/// profile with connectors on and a Nous sign-in.
fn with_connectors(inputs: &SurfaceInputsOwned<'_>, surface: ToolSurface) -> ToolSurface {
    let selected = inputs
        .selection
        .tools
        .iter()
        .any(|t| t == pmt_tools::connectors::manage::NAME);
    if inputs.agent.is_none() || !selected {
        return surface;
    }
    match crate::connectors_host::remote(inputs.scope) {
        Some(remote) => surface.with_remote(remote, &inputs.scope.config().tools.tool_search),
        None => surface,
    }
}

/// The session's dispatcher over its surface (recorded for `tools list
/// --session`) and the timings it reports to.
///
/// # Errors
///
/// Store errors recording the surface.
pub async fn dispatcher(
    inputs: &SurfaceInputsOwned<'_>,
) -> Result<(Dispatcher, Arc<Timings>), Error> {
    let config = inputs.scope.config();
    let surface = ToolSurface::build(SurfaceInputs {
        registry: inputs.registry,
        selection: inputs.selection,
        capabilities: inputs.capabilities,
        config,
        probes: &ProbeCache::new(),
        route: inputs.route,
    })
    .await;
    let surface = with_connectors(inputs, surface);
    crate::tools_cmd::record_session_surface(
        inputs.store,
        inputs.session,
        inputs.registry,
        &surface,
    )?;
    let session_id = inputs.session.to_string();
    let timings = Arc::new(Timings::default());
    let dispatcher = Dispatcher::new(DispatchSetup {
        session_id: session_id.clone(),
        surface: Arc::new(surface),
        capabilities: Arc::new(inputs.capabilities.clone()),
        hooks: Arc::clone(&inputs.hooks),
        gate: Arc::clone(inputs.gate) as _,
        observer: Arc::clone(&timings) as _,
        budget: ResultBudget::for_session(
            inputs.scope.dir(),
            &session_id,
            config,
            inputs.context_window,
            inputs.redactor.clone(),
        ),
        reporter: Arc::new(CrashReporter::for_home(
            inputs.scope.dir(),
            inputs.redactor.clone(),
        )),
        default_timeout: Duration::from_secs(config.tools.default_timeout_secs),
        concurrency: usize::try_from(config.agent.tool_concurrency)
            .unwrap_or(1)
            .max(1),
    });
    Ok((dispatcher, timings))
}

/// The session's tool port; a `--now` change swaps it at a turn boundary.
pub struct SwappableTools {
    current: RwLock<Arc<SessionTools>>,
    /// A note for the model put before the next tool result (a moved
    /// working directory), so the prompt itself never changes.
    announcement: std::sync::Mutex<Option<String>>,
    /// The session's form of `delegate_task` (fixed at creation).
    delegation: std::sync::OnceLock<pmt_delegation::shape::Shape>,
    /// Where each call's result goes into the session's recording.
    recording: std::sync::OnceLock<(Arc<pmt_obs::recording::Writer>, String)>,
    /// A replay's recorded results (spec: replay-and-evaluation, Replay
    /// modes).
    replay: std::sync::OnceLock<Arc<dyn crate::replay::ToolReplay>>,
    /// The restriction of a running `/plan` turn.
    plan: std::sync::Mutex<Option<super::plan::Plan>>,
    /// A working directory the `project` tool moved the session to,
    /// applied before its next batch (spec: projects, Project tool for GUI
    /// sessions).
    pending_move: std::sync::Mutex<Option<std::path::PathBuf>>,
    /// What applies such a move (the daemon's session; none elsewhere).
    mover: std::sync::OnceLock<Mover>,
}

/// Moves a session's working directory (rebuilding its environment).
pub type Mover =
    Arc<dyn Fn(std::path::PathBuf) -> BoxFuture<'static, Result<(), Error>> + Send + Sync>;

impl SwappableTools {
    /// A port over `tools`.
    #[must_use]
    pub fn new(tools: Arc<SessionTools>) -> Self {
        Self {
            current: RwLock::new(tools),
            announcement: std::sync::Mutex::default(),
            delegation: std::sync::OnceLock::new(),
            recording: std::sync::OnceLock::new(),
            replay: std::sync::OnceLock::new(),
            plan: std::sync::Mutex::default(),
            pending_move: std::sync::Mutex::default(),
            mover: std::sync::OnceLock::new(),
        }
    }

    /// Lets the session's working directory move (see
    /// [`Self::move_before_next_call`]).
    pub fn set_mover(&self, mover: Mover) {
        let _ = self.mover.set(mover);
    }

    /// Whether the session's working directory can move.
    #[must_use]
    pub fn can_move(&self) -> bool {
        self.mover.get().is_some()
    }

    /// Moves the session to `cwd` before its next tool call runs; the
    /// prompt and tool array stay as they are.
    pub fn move_before_next_call(&self, cwd: std::path::PathBuf) {
        *self
            .pending_move
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(cwd);
    }

    async fn apply_pending_move(&self) {
        let cwd = self
            .pending_move
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let (Some(cwd), Some(mover)) = (cwd, self.mover.get()) else {
            return;
        };
        if let Err(err) = mover(cwd.clone()).await {
            self.announce(format!(
                "[the session could not move to {}: {}]",
                cwd.display(),
                err.message()
            ));
        }
    }

    /// Restricts the calls of the next turn to `plan` (`/plan`).
    pub fn begin_plan(&self, plan: super::plan::Plan) {
        *self.plan.lock().unwrap_or_else(PoisonError::into_inner) = Some(plan);
    }

    /// Lifts the planning restriction (at the end of every turn); the plan
    /// that was in force.
    pub fn end_plan(&self) -> Option<super::plan::Plan> {
        self.plan
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

    /// The calls of `calls` the running `/plan` admits (all without one),
    /// as a mask in call order.
    fn plan_mask(&self, calls: &[RequestedCall]) -> Option<Vec<bool>> {
        let plan = self.plan.lock().unwrap_or_else(PoisonError::into_inner);
        let plan = plan.as_ref()?;
        let tools = self.tools();
        Some(
            calls
                .iter()
                .map(|call| plan.admits(call, tools.declared(&call.name).as_ref()))
                .collect(),
        )
    }

    /// Keeps every call's result in `session`'s recording.
    pub fn record_into(&self, writer: Arc<pmt_obs::recording::Writer>, session: &str) {
        let _ = self.recording.set((writer, session.to_owned()));
    }

    /// Answers calls from `replay` (a recorded result, or a live run).
    pub fn replay_from(&self, replay: Arc<dyn crate::replay::ToolReplay>) {
        let _ = self.replay.set(replay);
    }

    /// Runs one batch: replayed calls are answered from the recording,
    /// the rest run on the current tools; every result is recorded.
    async fn run_batch(
        &self,
        calls: &[RequestedCall],
        turn_id: &str,
        cancel: &CancelToken,
        journal: Option<Arc<dyn pmt_agent::turn::CallJournal>>,
    ) -> ToolBatch {
        self.apply_pending_move().await;
        if let Some(mask) = self.plan_mask(calls)
            && mask.contains(&false)
        {
            return Box::pin(self.run_planned(calls, &mask, turn_id, cancel, journal)).await;
        }
        let tools = self.tools();
        let batch = match self.replay.get() {
            Some(replay) => {
                replayed(replay.as_ref(), &tools, (calls, turn_id), cancel, journal).await
            }
            None => run_on(&tools, calls, turn_id, cancel, journal).await,
        };
        self.record(calls, &batch);
        self.with_announcement(batch)
    }

    /// A batch under `/plan`: the admitted calls run, the others are
    /// refused, the outcomes stay in call order.
    async fn run_planned(
        &self,
        calls: &[RequestedCall],
        mask: &[bool],
        turn_id: &str,
        cancel: &CancelToken,
        journal: Option<Arc<dyn pmt_agent::turn::CallJournal>>,
    ) -> ToolBatch {
        let admitted: Vec<RequestedCall> = calls
            .iter()
            .zip(mask)
            .filter(|(_, ok)| **ok)
            .map(|(call, _)| call.clone())
            .collect();
        let ran = if admitted.is_empty() {
            ToolBatch::default()
        } else {
            Box::pin(self.run_batch(&admitted, turn_id, cancel, journal)).await
        };
        let mut ran = ran.outcomes.into_iter();
        let outcomes = mask
            .iter()
            .map(|ok| {
                if *ok {
                    ran.next().unwrap_or_else(super::plan::refused)
                } else {
                    super::plan::refused()
                }
            })
            .collect();
        ToolBatch {
            outcomes,
            plan: None,
        }
    }

    fn record(&self, calls: &[RequestedCall], batch: &ToolBatch) {
        let Some((writer, session)) = self.recording.get() else {
            return;
        };
        for (call, outcome) in calls.iter().zip(&batch.outcomes) {
            writer.tool(
                session,
                &pmt_obs::recording::Tool {
                    ts: jiff::Timestamp::now().to_string(),
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: serde_json::from_str(&call.arguments)
                        .unwrap_or_else(|_| serde_json::Value::String(call.arguments.clone())),
                    result: serde_json::to_value(&outcome.result).unwrap_or_default(),
                },
            );
        }
    }

    /// Offers `delegate_task` in `shape`'s form.
    pub fn shape_delegation(&self, shape: pmt_delegation::shape::Shape) {
        let _ = self.delegation.set(shape);
    }

    /// Puts `note` before the next tool result the model gets.
    pub fn announce(&self, note: String) {
        *self
            .announcement
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(note);
    }

    /// Serves `tools` from the next turn.
    pub fn swap(&self, tools: Arc<SessionTools>) {
        *self.current.write().unwrap_or_else(PoisonError::into_inner) = tools;
    }

    fn tools(&self) -> Arc<SessionTools> {
        Arc::clone(&self.current.read().unwrap_or_else(PoisonError::into_inner))
    }

    /// Puts a pending toolset announcement before the batch's first result.
    fn with_announcement(&self, mut batch: ToolBatch) -> ToolBatch {
        if let Some(first) = batch.outcomes.first_mut()
            && let Some(note) = self
                .announcement
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
        {
            first
                .result
                .content
                .insert(0, pmt_tools::result::Content::Text { text: note });
        }
        batch
    }
}

impl ToolPort for SwappableTools {
    fn definitions(&self) -> Vec<Tool> {
        let mut tools = self.tools().definitions();
        if let Some(replay) = self.replay.get() {
            tools = replay_definitions(tools, replay.recorded_tools());
        }
        let shape = self.delegation.get().copied().unwrap_or_default();
        for tool in &mut tools {
            if pmt_delegation::shape::Shape::applies_to(&tool.name) {
                shape.tailor(&mut tool.parameters);
            }
        }
        tools
    }

    fn idempotent(&self, name: &str) -> bool {
        self.tools().idempotent(name)
    }

    fn toolset_of(&self, name: &str) -> Option<(String, bool)> {
        self.tools().toolset_of(name)
    }

    fn run<'a>(
        &'a self,
        calls: &'a [RequestedCall],
        turn_id: &'a str,
        cancel: &'a CancelToken,
    ) -> BoxFuture<'a, ToolBatch> {
        Box::pin(self.run_batch(calls, turn_id, cancel, None))
    }

    fn run_journaled<'a>(
        &'a self,
        calls: &'a [RequestedCall],
        turn_id: &'a str,
        cancel: &'a CancelToken,
        journal: Arc<dyn pmt_agent::turn::CallJournal>,
    ) -> BoxFuture<'a, ToolBatch> {
        Box::pin(self.run_batch(calls, turn_id, cancel, Some(journal)))
    }

    fn declared(&self, name: &str) -> Option<pmt_agent::turn::Declared> {
        self.tools().declared(name)
    }

    fn settle(&self) -> BoxFuture<'_, Option<String>> {
        let tools = self.tools();
        Box::pin(async move { tools.settle().await })
    }
}

/// `calls` on `tools`, journaled when there is a journal.
async fn run_on(
    tools: &SessionTools,
    calls: &[RequestedCall],
    turn_id: &str,
    cancel: &CancelToken,
    journal: Option<Arc<dyn pmt_agent::turn::CallJournal>>,
) -> ToolBatch {
    match journal {
        Some(journal) => tools.run_journaled(calls, turn_id, cancel, journal).await,
        None => tools.run(calls, turn_id, cancel).await,
    }
}

/// A replayed batch: recorded results where the replay has them, the
/// other calls run on `tools`; the plan is the one `tools` would run by.
async fn replayed(
    replay: &dyn crate::replay::ToolReplay,
    tools: &SessionTools,
    (calls, turn_id): (&[RequestedCall], &str),
    cancel: &CancelToken,
    journal: Option<Arc<dyn pmt_agent::turn::CallJournal>>,
) -> ToolBatch {
    use crate::replay::ToolAnswer;
    let answers: Vec<ToolAnswer> = calls.iter().map(|call| replay.answer(call)).collect();
    let live: Vec<RequestedCall> = calls
        .iter()
        .zip(&answers)
        .filter(|(_, answer)| matches!(answer, ToolAnswer::Live))
        .map(|(call, _)| call.clone())
        .collect();
    let mut ran = if live.is_empty() {
        Vec::new()
    } else {
        run_on(tools, &live, turn_id, cancel, journal.clone())
            .await
            .outcomes
    }
    .into_iter();
    let outcomes = calls
        .iter()
        .zip(answers)
        .map(|(call, answer)| match answer {
            ToolAnswer::Serve(result) => {
                if let Some(journal) = &journal {
                    journal.started(&call.id);
                    journal.finished(&call.id, &result);
                }
                pmt_agent::turn::tools::ToolOutcome {
                    result,
                    duration_ms: 0,
                }
            }
            ToolAnswer::Live => ran
                .next()
                .unwrap_or_else(|| pmt_agent::turn::tools::ToolOutcome {
                    result: pmt_tools::ToolResult::error("the call did not run"),
                    duration_ms: 0,
                }),
        })
        .collect();
    ToolBatch {
        outcomes,
        plan: Some(tools.plan(calls, turn_id)),
    }
}

/// A replay offers the tools its recording offered, in recorded order:
/// this build's schema of each where it has the tool (a schema change is
/// what a replay must catch), the recorded one where it does not (plugin
/// and MCP tools a replay starts without). The session's own selection
/// (a scheduled run's toolsets, its memory mode) is the recording's.
fn replay_definitions(current: Vec<Tool>, recorded: Vec<Tool>) -> Vec<Tool> {
    if recorded.is_empty() {
        return current;
    }
    recorded
        .into_iter()
        .map(|tool| {
            current
                .iter()
                .find(|t| t.name == tool.name)
                .cloned()
                .unwrap_or(tool)
        })
        .collect()
}

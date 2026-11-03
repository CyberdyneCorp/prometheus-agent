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
    /// `--yolo`.
    pub yolo: bool,
    /// Plugin approval observers and transport.
    pub hooks: Option<Arc<dyn pmt_tools::approval::hooks::ApprovalHooks>>,
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
    } else {
        (Channel::unattended(), None, Context::SingleQuery)
    };
    let binary = std::env::current_exe().ok();
    let floors = Floors::new(inputs.cwd)
        .user_home(inputs.user_home)
        .prometheus_home(inputs.scope.home())
        .binary(binary.as_deref());
    let gate = CommandGate::new(GateSetup {
        approvals: config.approvals.clone(),
        context,
        floors,
        grants: Arc::clone(inputs.grants),
        protected: ProtectedFiles::from_config(&config.security)?,
        channel,
        yolo: inputs.yolo,
        hooks: inputs.hooks.clone(),
    });
    Ok(Gate {
        gate: Arc::new(gate),
        prompts,
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
    config: &Config,
    interactive: bool,
    plugins: &[String],
    surface: &SessionSurface,
) -> Result<Selection, Error> {
    let mut enabled = surface.starting_toolsets(registry, &config.tools, &config.toolsets);
    if !surface.is_webhook() {
        let opted_in: Vec<String> = plugins
            .iter()
            .filter(|p| !enabled.contains(p) && !pmt_config::platforms::is_known(p))
            .cloned()
            .collect();
        enabled.extend(opted_in);
    }
    let selection = Selection::resolve(registry, &enabled, &config.tools.disabled_toolsets)?;
    Ok(if interactive {
        selection
    } else {
        selection.without_interactive(registry)
    })
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

/// The schema-variant traits of `model`'s current route.
pub async fn route_traits(model: &dyn pmt_agent::turn::model::ModelPort) -> RouteTraits {
    let route = model.route().await;
    RouteTraits::of(&route.provider, &route.model, route.wire_format)
        .with_context_window(route.context_window)
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
}

impl SwappableTools {
    /// A port over `tools`.
    #[must_use]
    pub fn new(tools: Arc<SessionTools>) -> Self {
        Self {
            current: RwLock::new(tools),
        }
    }

    /// Serves `tools` from the next turn.
    pub fn swap(&self, tools: Arc<SessionTools>) {
        *self.current.write().unwrap_or_else(PoisonError::into_inner) = tools;
    }

    fn tools(&self) -> Arc<SessionTools> {
        Arc::clone(&self.current.read().unwrap_or_else(PoisonError::into_inner))
    }
}

impl ToolPort for SwappableTools {
    fn definitions(&self) -> Vec<Tool> {
        self.tools().definitions()
    }

    fn idempotent(&self, name: &str) -> bool {
        self.tools().idempotent(name)
    }

    fn run<'a>(
        &'a self,
        calls: &'a [RequestedCall],
        turn_id: &'a str,
        cancel: &'a CancelToken,
    ) -> BoxFuture<'a, ToolBatch> {
        let tools = self.tools();
        Box::pin(async move { tools.run(calls, turn_id, cancel).await })
    }
}

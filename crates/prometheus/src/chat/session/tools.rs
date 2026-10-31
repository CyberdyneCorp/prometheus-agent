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
use pmt_tools::dispatch::NoHooks;
use pmt_tools::probe::ProbeCache;
use pmt_tools::protected::ProtectedFiles;
use pmt_tools::{DispatchSetup, Dispatcher, Selection, SurfaceInputs, ToolRegistry, ToolSurface};

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
    });
    Ok(Gate {
        gate: Arc::new(gate),
        prompts,
    })
}

/// The tool selection of a session: `tools.enabled_toolsets` (or
/// `--toolsets`) minus the disabled toolsets; a non-interactive run drops
/// the tools that need a person.
///
/// # Errors
///
/// An unknown toolset (`registry.unknown_name`, exit 78).
pub fn selection(
    registry: &ToolRegistry,
    config: &Config,
    interactive: bool,
) -> Result<Selection, Error> {
    let selection = Selection::resolve(
        registry,
        &config.tools.enabled_toolsets,
        &config.tools.disabled_toolsets,
    )?;
    Ok(if interactive {
        selection
    } else {
        selection.without_interactive(registry)
    })
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
}

/// Builds the surface and the dispatcher over it, and records the
/// surface for `tools list --session`.
///
/// # Errors
///
/// Store errors recording the surface.
pub async fn session_tools(inputs: &SurfaceInputsOwned<'_>) -> Result<Arc<SessionTools>, Error> {
    let config = inputs.scope.config();
    let surface = ToolSurface::build(SurfaceInputs {
        registry: inputs.registry,
        selection: inputs.selection,
        capabilities: inputs.capabilities,
        config,
        probes: &ProbeCache::new(),
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
        hooks: Arc::new(NoHooks),
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
    Ok(Arc::new(SessionTools::new(
        inputs.agent.map(Arc::clone),
        Arc::new(dispatcher),
        timings,
    )))
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
        cancel: &'a CancelToken,
    ) -> BoxFuture<'a, ToolBatch> {
        let tools = self.tools();
        Box::pin(async move { tools.run(calls, cancel).await })
    }
}

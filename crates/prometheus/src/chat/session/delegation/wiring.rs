//! Builds a session's delegation as the session opens: its frozen
//! context (config, agent definitions, router, memory snapshot, dispatch
//! mode, one-shot budget), the port `delegate_task` runs through, and the
//! late facts only known once the session's tools and events exist.

use std::sync::{Arc, OnceLock};

use pmt_agent::tools::{AgentTools, DelegationPort};
use pmt_agent::turn::model::BoxFuture;
use pmt_agent::turn::{EventSink, ToolPort as _};
use pmt_core::cancel::CancelToken;
use pmt_core::capability::CapabilitySet;
use pmt_delegation::agents::Definitions;
use pmt_delegation::budget::OneShotBudget;
use pmt_delegation::grant::ParentGrant;
use pmt_delegation::host::{Delegator, Dispatch, SessionContext};
use pmt_obs::trajectory::writer::Emit;
use pmt_store::{Source, Visibility};
use pmt_tools::ToolResult;
use pmt_tools::approval::CommandGate;
use serde_json::Value;

use super::super::tools::SwappableTools;
use super::super::{Clients, Factory};
use super::{ChildLauncher, ChildSpec};

/// The file whose presence pauses spawning for a profile
/// (`/delegation pause`).
pub const PAUSE_FILE: &str = "delegation.paused";

/// A session's delegation while it opens.
pub struct Wiring {
    delegator: Arc<Delegator>,
    code: Option<Arc<super::super::code::SessionCode>>,
    events: Arc<LateEvents>,
    gate: Arc<OnceLock<Arc<CommandGate>>>,
}

impl Wiring {
    /// The delegation of `session` (a child when `child` is set).
    pub fn new(
        factory: &Factory,
        session: &pmt_store::Session,
        model: &Arc<super::super::model::SwitchableModel>,
        registry: &Arc<pmt_tools::ToolRegistry>,
        child: Option<&ChildSpec>,
        clients: Option<&Clients>,
        memory: Option<String>,
    ) -> Self {
        let scope = factory.scope.clone();
        let config = scope.config().delegation.clone();
        let events = Arc::new(LateEvents::default());
        if let Some(spec) = child {
            let _ = events.0.set(Arc::clone(&spec.root_events));
        }
        let gate: Arc<OnceLock<Arc<CommandGate>>> = Arc::default();
        let oneshot = oneshot(factory, session, child, config.oneshot_max_children);
        let launcher = ChildLauncher::new(
            factory,
            Arc::clone(model),
            Arc::clone(&gate),
            Arc::clone(&events) as _,
            oneshot.clone(),
        );
        let dispatch = dispatch(factory, session, child, clients);
        let paused_file = scope.dir().join(PAUSE_FILE);
        let store = Arc::clone(&factory.store);
        let id = session.id;
        let window = factory
            .models
            .catalog()
            .metadata(
                &model.target().provider,
                model.target().model.as_deref().unwrap_or_default(),
            )
            .0
            .context_window;
        let ctx = SessionContext {
            store: Arc::clone(&factory.store),
            session: session.id,
            root: session.root_session_id,
            depth: child.map_or(0, |c| c.depth),
            registry: Arc::clone(registry),
            agents: Definitions::load(&[scope.dir().join("agents")]),
            router: super::routers::select(
                &config.router,
                &factory.plugins,
                &factory.plugin_call(session.id),
            ),
            memory: child.map_or(memory, |c| c.memory.clone()),
            dispatch,
            oneshot,
            events: Arc::clone(&events) as _,
            launcher: Arc::new(launcher),
            spill_dir: pmt_tools::spillover::session_dir(scope.dir(), &id.to_string()),
            paused: Arc::new(move || paused_file.exists()),
            context_left: Arc::new(move || Some(context_left(&store, id, window))),
            config,
            scope,
        };
        Self {
            delegator: Delegator::new(ctx),
            code: None,
            events,
            gate,
        }
    }

    /// Runs the session's `execute_code` through `code` too.
    pub fn attach_code(&mut self, code: Arc<super::super::code::SessionCode>) {
        self.code = Some(code);
    }

    /// `tools` with `delegate_task` (and `execute_code`) running through
    /// this session.
    #[must_use]
    pub fn equip(&self, tools: AgentTools) -> AgentTools {
        let tools = tools.with_delegation(Arc::new(Port(Arc::clone(&self.delegator))));
        match &self.code {
            Some(code) => tools.with_code(Arc::clone(code) as _),
            None => tools,
        }
    }

    /// What the session holds, once its tools are built.
    pub fn parent_is(
        &self,
        tools: &Arc<SwappableTools>,
        capabilities: &CapabilitySet,
        gate: &Arc<CommandGate>,
    ) {
        if let Some(code) = &self.code {
            code.bind(tools);
        }
        let tools = tools.definitions().into_iter().map(|t| t.name).collect();
        self.delegator.set_parent(ParentGrant {
            tools,
            capabilities: capabilities.clone(),
        });
        let _ = self.gate.set(Arc::clone(gate));
    }

    /// The session's own event stream (a top-level session's children
    /// report there).
    pub fn events_are(&self, events: &Arc<dyn EventSink>) {
        let _ = self.events.0.set(Arc::clone(events));
    }

    /// The delegation.
    #[must_use]
    pub fn delegator(&self) -> Option<Arc<Delegator>> {
        Some(Arc::clone(&self.delegator))
    }
}

/// Tokens left in the session's context window: the window less a
/// 4-characters-a-token estimate of its active messages.
fn context_left(store: &pmt_store::Store, id: pmt_core::id::SessionId, window: u64) -> u64 {
    let used: u64 = store.messages(id, Visibility::Active).map_or(0, |rows| {
        rows.iter()
            .filter_map(|m| m.content.as_ref())
            .map(|c| u64::try_from(c.len()).unwrap_or(u64::MAX))
            .sum()
    });
    window.saturating_sub(used / pmt_delegation::summary::CHARS_PER_TOKEN)
}

/// How the session's spawns run: in the background where a sink takes
/// later completions (daemon sessions of interactive clients and gateway
/// chats), otherwise synchronously, saying why.
fn dispatch(
    factory: &Factory,
    session: &pmt_store::Session,
    child: Option<&ChildSpec>,
    clients: Option<&Clients>,
) -> Dispatch {
    if child.is_some() {
        return Dispatch::Synchronous("Nested spawns run synchronously.".to_owned());
    }
    let sink = clients
        .and_then(|c| c.delegation.clone())
        .or_else(|| factory.delegation_inbox.clone());
    if let Some(sink) = sink {
        return Dispatch::Background(sink);
    }
    let why = match session.source {
        Source::Cron => "Scheduled jobs wait for their children.",
        Source::Api => "Stateless API calls wait for their children.",
        _ if is_oneshot(factory) => "One-shot runs wait for their children.",
        _ if is_kanban(session) => "Kanban workers wait for their children.",
        _ => "This session cannot receive later completions, so the call waited for its children.",
    };
    Dispatch::Synchronous(why.to_owned())
}

fn is_oneshot(factory: &Factory) -> bool {
    factory.options.source_tag.as_deref() == Some(pmt_store::sessions::ONESHOT_TAG)
}

fn is_kanban(session: &pmt_store::Session) -> bool {
    session
        .source_tag
        .as_deref()
        .is_some_and(|tag| tag.starts_with(super::super::KANBAN_SOURCE_TAG))
}

/// The spawn budget of a finite run (`chat -z`, cron, eval); children
/// share their parent's.
fn oneshot(
    factory: &Factory,
    session: &pmt_store::Session,
    child: Option<&ChildSpec>,
    max: u32,
) -> Option<Arc<OneShotBudget>> {
    if let Some(spec) = child {
        return spec.oneshot.clone();
    }
    let finite = is_oneshot(factory) || matches!(session.source, Source::Cron | Source::Eval);
    finite.then(|| Arc::new(OneShotBudget::new(max)))
}

/// `delegate_task` of one session.
struct Port(Arc<Delegator>);

impl DelegationPort for Port {
    fn call<'a>(&'a self, arguments: Value, cancel: &'a CancelToken) -> BoxFuture<'a, ToolResult> {
        Box::pin(async move { self.0.call(&arguments, cancel).await })
    }
}

/// The root session's event stream, known only once the session's sink
/// exists (children are never spawned before that).
#[derive(Default)]
pub(super) struct LateEvents(OnceLock<Arc<dyn EventSink>>);

impl EventSink for LateEvents {
    fn emit(&self, emit: Emit) -> BoxFuture<'_, Result<(), pmt_core::Error>> {
        match self.0.get() {
            Some(sink) => sink.emit(emit),
            None => Box::pin(async { Ok(()) }),
        }
    }
}

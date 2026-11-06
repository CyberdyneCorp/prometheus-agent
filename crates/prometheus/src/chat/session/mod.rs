//! Session start for the CLI (spec: cli, agent-turn-loop, tool-system):
//! everything one chat session runs with, built once per session and torn
//! down when it ends. [`Factory`] holds what every session of the process
//! shares (profile, store, model side, tool registry, grants);
//! [`Factory::open`] builds one [`Live`] session over a new or resumed
//! session row.

pub mod api_hooks;
pub mod backend;
pub mod browser;
pub mod code;
pub mod delegation;
pub mod documents;
pub mod downgrades;
pub mod env;
mod guardian;
mod lifecycle;
mod lsp;
mod mcp;
pub mod modal;
pub mod model;
mod opening;
pub mod place;
mod plugins;
pub mod processes;
pub mod progress;
pub mod python;
pub mod serve;
pub mod skills;
pub mod snapshots;
pub mod sudo;
pub mod summarize;
mod tool_state;
pub mod tools;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use jiff::Timestamp;
use pmt_agent::tools::clarify::{Clarifier, ClarifyAnswer, ClarifyRequest};
use pmt_agent::tools::{AgentTools, SessionSearch};
use pmt_agent::turn::{
    Agent, AgentSetup, EventSink, NoEvents, PromptSetup, RefreshPrompt, SessionTools,
};
use pmt_config::grants::{GrantsFile, SessionGrants, Subject, baseline};
use pmt_config::{ProfileScope, SharedScope};
use pmt_core::capability::CapabilitySet;
use pmt_core::env::LaunchEnv;
use pmt_core::error::Error;
use pmt_core::human::Surface;
use pmt_core::id::SessionId;
use pmt_core::secret::Redactor;
use pmt_memory::session::{Approver, MemorySession, MemorySetup};
use pmt_obs::trajectory::events::Event;
use pmt_obs::trajectory::payload::{SessionEnd, SessionStart};
use pmt_obs::trajectory::writer::{Emit, TrajectoryWriter};
use pmt_skills::review::Nudge;
use pmt_skills::secrets::SecretPrompt;
use pmt_skills::session::{SessionSetup, SkillSessions, SkillsSession};
use pmt_store::{EndReason, NewSession, Source, Store};
use pmt_tools::approval::{ApprovalRequest, CommandGate, Decision};
use pmt_tools::environments::tool_output_reads;
use pmt_tools::{Selection, ToolRegistry};

use self::env::Denials;
use self::model::{Models, RouteFlags, SwitchableModel};
use self::tool_state::{Registries, ToolState};
use self::tools::{GateInputs, SurfaceInputsOwned, SwappableTools};

/// Clarify questions waiting for the person.
pub type ClarifyPrompts = Surface<ClarifyRequest, ClarifyAnswer>;

/// What the chat flags decide about every session of this process.
#[derive(Debug, Clone, Default)]
#[allow(clippy::struct_excessive_bools)] // independent per-invocation switches
pub struct Options {
    /// Someone can answer prompts (an interactive line session).
    pub interactive: bool,
    /// `--yolo`.
    pub yolo: bool,
    /// The session's working directory.
    pub cwd: PathBuf,
    /// `-s/--skills`.
    pub skills: Vec<String>,
    /// `--pass-session-id`.
    pub pass_session_id: bool,
    /// `--no-context-files`.
    pub no_context_files: bool,
    /// `--source` (or `oneshot` for a one-shot run).
    pub source_tag: Option<String>,
    /// `--ignore-user-config` (recorded on the session).
    pub ignore_user_config: bool,
    /// `--ignore-rules`: no rule files, identity file, memory snapshot or
    /// preloaded skills (recorded on the session).
    pub ignore_rules: bool,
    /// `--egress` and `--allow-host`.
    pub egress: crate::egress::Overrides,
}

/// The clients of one daemon session: they see its events and answer its
/// secret prompts (a factory serves several sessions, each with its own).
#[derive(Clone)]
pub struct Clients {
    /// Where the session's trajectory events also go.
    pub events: Arc<dyn EventSink>,
    /// Asks the session's clients for a skill's missing secret.
    pub secrets: Arc<dyn SecretPrompt>,
    /// Where the terminal reports finished `notify_on_complete` background
    /// processes (a gateway session's chat).
    pub completions:
        Option<tokio::sync::mpsc::UnboundedSender<pmt_tools::terminal::process::Notice>>,
    /// Asks the session's clients for the sudo password (`sudo.request`);
    /// `None` where nobody can answer securely.
    pub sudo: Option<Arc<dyn pmt_tools::terminal::sudo::SudoPrompt>>,
    /// Where the session's background delegations deliver their
    /// completions; `None`: its spawns wait for their children.
    pub delegation: Option<Arc<dyn pmt_delegation::host::Completions>>,
}

/// What every session of this process shares.
pub struct Factory {
    /// The profile.
    pub scope: ProfileScope,
    /// The profile's store.
    pub store: Arc<Store>,
    /// The model side.
    pub models: Models,
    /// The core tools plus those of the current plugin and MCP server
    /// sets; read through [`Factory::session_registry`].
    registries: Arc<Registries>,
    grants: Arc<SessionGrants>,
    launch: LaunchEnv,
    redactor: Redactor,
    /// What this run did (`--usage-file`).
    pub record: Arc<crate::chat::usage_file::RunRecord>,
    /// The flags.
    pub options: Options,
    approver: Option<Arc<dyn Approver>>,
    secret_prompt: Option<Arc<dyn SecretPrompt>>,
    /// Where the background processes of an interactive line session
    /// report (printed as their own messages).
    notices: Option<tokio::sync::mpsc::UnboundedSender<pmt_tools::terminal::process::Notice>>,
    maintained: std::sync::atomic::AtomicBool,
    install_asks: Option<tokio::sync::mpsc::UnboundedSender<browser::InstallAsk>>,
    /// The browser tools every session shares, set up by the first
    /// session (`None`: they could not be, see the WARN).
    browser: OnceLock<Option<browser::Shared>>,
    /// The gateway's `send_message` ports (daemon sessions only).
    messaging: Option<Arc<dyn pmt_agent::tools::MessagingPorts>>,
    /// The daemon hosting this factory's sessions (daemon sessions only).
    daemon: Option<pmt_tools::approval::selfterm::Daemon>,
    /// The profile's configuration as it is now: `scope` until a daemon
    /// reload replaces it (shared tools read their `live` keys from it).
    current: SharedScope,
    /// The profile's live plugin set.
    plugins: crate::plugins_host::ProfilePlugins,
    /// The configuration the plugin set is loaded from: `scope` until a
    /// one-process `/plugins reload` re-reads `config.toml`.
    plugin_scope: SharedScope,
    /// The `--set` list a one-process session opened `scope` with, so
    /// `/plugins reload` can re-read `config.toml` through it (`None`: the
    /// daemon, whose own reloads keep the profile scope current).
    reread: Option<Vec<String>>,
    /// The route flags the factory was opened with (its delegated children
    /// open their own factories with them).
    route_flags: delegation::OwnedFlags,
    /// Where an in-process interactive chat takes background delegation
    /// completions (a daemon session gets its sink from its clients).
    delegation_inbox: Option<Arc<dyn pmt_delegation::host::Completions>>,
}

/// One running session.
pub struct Live {
    /// The profile.
    pub scope: ProfileScope,
    /// The session.
    pub id: SessionId,
    /// The turn loop.
    pub agent: Agent,
    /// Keeps the session reachable by the profile's MCP servers.
    _mcp: crate::mcp_host::sessions::Registration,
    /// The model port.
    pub model: Arc<SwitchableModel>,
    /// The tool port.
    pub tools: Arc<SwappableTools>,
    /// The approval gate.
    pub gate: Arc<CommandGate>,
    /// Approval prompts to show (interactive only).
    pub approvals: Option<Surface<ApprovalRequest, Decision>>,
    /// Clarify questions to show (interactive only).
    pub clarify: Option<ClarifyPrompts>,
    /// The session's skills.
    pub skills: Arc<SkillsSession>,
    /// The session's memory.
    pub memory: Arc<MemorySession>,
    /// Counts tool-calling iterations towards a background skill review.
    pub skill_nudge: Mutex<Nudge>,
    /// The registry, selection and main route the session's tools are
    /// built from, and their rebuild (`--now`, a compaction boundary).
    tool_state: Arc<ToolState>,
    /// The surface the session's toolsets start from.
    session_surface: pmt_tools::SessionSurface,
    /// The profile's live plugin set (slash commands, reload).
    pub plugins: crate::plugins_host::ProfilePlugins,
    /// Where the session works: its cwd and cwd-bound environment
    /// (`/worktree new` moves it).
    pub place: place::Place,
    /// The daemon clients it was opened with (a moved environment's
    /// prompts and notices go to them too).
    clients: Option<Clients>,
    /// The surface that created the session (`cli`, `tui`, `gateway`...).
    surface: &'static str,
    /// The plugin system-prompt sections the session's prompt carries.
    plugin_sections: Arc<Mutex<Vec<String>>>,
    /// Keeps the session's model and delegation reachable for plugin calls.
    _plugin: plugins::Registrations,
    /// Notices to show once at start.
    pub notices: Vec<String>,
    /// What this run did (`--usage-file`).
    pub record: Arc<crate::chat::usage_file::RunRecord>,
    /// Tool activity to render (taken by the interface).
    pub progress: Option<tokio::sync::mpsc::UnboundedReceiver<progress::Progress>>,
    /// The configuration the session runs with now: `scope`, with the
    /// `live` keys of each reload a daemon applies (spec: configuration-
    /// and-profiles, Live reload with apply classes). Turns and tools
    /// read it at use.
    pub config: SharedScope,
    capabilities: CapabilitySet,
    trajectory: Option<Arc<TrajectoryWriter>>,
    /// The session's egress policy (`/egress`, `egress.set`).
    pub egress: crate::egress::Running,
    /// Its allowed hardening downgrades (banner marker, `security.downgrade`).
    pub downgrades: downgrades::Downgrades,
    /// The session's subagent delegation (`/stop all`, close, `/agents`).
    pub delegation: Option<Arc<pmt_delegation::host::Delegator>>,
    _skills: pmt_skills::session::Attachment,
}

impl Factory {
    /// What every session of the profile shares.
    ///
    /// # Errors
    ///
    /// Store, registry, model and grants errors.
    pub fn new(
        scope: ProfileScope,
        launch: &LaunchEnv,
        flags: &RouteFlags<'_>,
        ad_hoc: Option<pmt_providers::resolve::AdHoc>,
        options: Options,
    ) -> Result<Self, Error> {
        let store = Arc::new(Store::open(&scope)?);
        Self::with_store(scope, store, launch, flags, ad_hoc, options)
    }

    /// Like [`Factory::new`] over an open `store`: a delegated child routed
    /// to another profile keeps its session record and usage in the
    /// launching profile's store.
    ///
    /// # Errors
    ///
    /// As [`Factory::new`].
    pub fn with_store(
        scope: ProfileScope,
        store: Arc<Store>,
        launch: &LaunchEnv,
        flags: &RouteFlags<'_>,
        ad_hoc: Option<pmt_providers::resolve::AdHoc>,
        options: Options,
    ) -> Result<Self, Error> {
        let config = scope.config();
        // Settings only a session acts on fail here, before any turn
        // (spec: execution-environments, Typed backend registry and
        // selection; memory, Context engine selection).
        pmt_agent::compress::engine::select(&config.context, &[])?;
        let user_home = launch.user_home().map(PathBuf::as_path);
        let plugins = crate::plugins_host::for_session(&scope, &options.cwd);
        backend::registry(&plugins, &pmt_plugins::host::CallInfo::default())
            .select(&config.terminal.backend)?;
        let models = Models::open(&scope, &store, user_home, flags, ad_hoc)?.with_plugins(
            &scope,
            &plugins,
            &options.cwd,
        )?;
        let plugin_scope = SharedScope::new(scope.clone());
        let registries = Arc::new(Registries::new(
            scope.clone(),
            plugin_scope.clone(),
            Arc::clone(&store),
            options.cwd.clone(),
            plugins::PluginTools::build(
                &scope,
                &plugins,
                &crate::mcp_host::manager(&scope),
                &plugins::session_calls(&store, &options.cwd),
            )?,
        ));
        let grants = Arc::new(SessionGrants::new(
            baseline(&scope.config().security),
            GrantsFile::of(scope.profile()),
        ));
        let redactor = scope.secrets().redactor().unwrap_or_default();
        Ok(Self {
            current: SharedScope::new(scope.clone()),
            plugin_scope,
            scope,
            store,
            models,
            registries,
            grants,
            launch: launch.clone(),
            redactor,
            record: Arc::default(),
            options,
            approver: None,
            secret_prompt: None,
            notices: None,
            maintained: std::sync::atomic::AtomicBool::new(false),
            install_asks: None,
            browser: OnceLock::new(),
            messaging: None,
            daemon: None,
            plugins,
            reread: None,
            route_flags: delegation::OwnedFlags::of(flags),
            delegation_inbox: None,
        })
    }

    /// Reads the profile's `live` keys from `current` (a daemon's
    /// reloaded configuration) instead of the snapshot it was built with.
    #[must_use]
    pub fn with_current(mut self, current: SharedScope) -> Self {
        self.current = current;
        self
    }

    /// Sends runtime install questions (`Install <name> <version>
    /// (<size>)? [Y/n]`) to `asks`; without it, missing runtimes are
    /// installed without asking.
    #[must_use]
    pub fn with_install_asks(
        mut self,
        asks: tokio::sync::mpsc::UnboundedSender<browser::InstallAsk>,
    ) -> Self {
        self.install_asks = Some(asks);
        self
    }

    /// The shared browser tools, set up on first use.
    pub(crate) fn browser(&self) -> Option<&browser::Shared> {
        self.browser
            .get_or_init(|| {
                browser::Shared::new(
                    &self.scope,
                    &self.current,
                    &self.launch,
                    &self.redactor,
                    self.install_asks.clone(),
                )
                .inspect_err(|err| tracing::warn!(error = %err, "browser tools unavailable"))
                .ok()
            })
            .as_ref()
    }

    /// `selection` with `browser_cdp` and `browser_dialog` opted in when
    /// session `id` ran `/browser connect` (spec: execution-environments,
    /// `browser_cdp` and `browser_dialog` tools): another session's
    /// connection never offers them. An unset browser has no connection.
    fn with_browser_opt_ins(&self, id: SessionId, selection: Selection) -> Selection {
        let connected = self
            .browser
            .get()
            .and_then(Option::as_ref)
            .is_some_and(|shared| shared.manager().connected(&id.to_string()));
        if connected {
            selection.with_session_opt_ins(pmt_tools::browser::cdp_tools::NAMES)
        } else {
            selection
        }
    }

    /// Whether the model of `target` accepts images.
    pub(super) fn image_input(&self, target: &pmt_providers::resolve::Target) -> bool {
        target.model.as_deref().is_some_and(|model| {
            self.models
                .catalog()
                .metadata(&target.provider, model)
                .0
                .supports_vision
        })
    }

    /// Sends the background process notices of this factory's sessions
    /// to `notices` (an interactive line session prints them).
    #[must_use]
    pub fn with_notices(
        mut self,
        notices: tokio::sync::mpsc::UnboundedSender<pmt_tools::terminal::process::Notice>,
    ) -> Self {
        self.notices = Some(notices);
        self
    }

    /// Delivers the background delegations of this factory's sessions to
    /// `inbox` (an in-process line chat runs them as its next input).
    #[must_use]
    pub fn with_delegation_inbox(
        mut self,
        inbox: Arc<dyn pmt_delegation::host::Completions>,
    ) -> Self {
        self.delegation_inbox = Some(inbox);
        self
    }

    /// Asks the person for a skill's missing secrets with `prompt`
    /// (interactive surfaces).
    #[must_use]
    pub fn with_secret_prompt(mut self, prompt: Arc<dyn SecretPrompt>) -> Self {
        self.secret_prompt = Some(prompt);
        self
    }

    /// Starts the profile's daily maintenance (session pruning and
    /// auto-archiving, blob collection, trajectory retention) once per
    /// process, off the prompt's path; it throttles itself to once a day.
    fn maintain(&self) {
        use std::sync::atomic::Ordering;
        if self.maintained.swap(true, Ordering::Relaxed) {
            return;
        }
        let (store, scope) = (Arc::clone(&self.store), self.scope.clone());
        tokio::task::spawn_blocking(move || {
            match pmt_obs::trajectory::retention::daily(&store, &scope) {
                Ok(Some(swept)) => tracing::debug!(
                    purged = swept.purged.len(),
                    orphans = swept.orphans.len(),
                    "daily maintenance ran"
                ),
                Ok(None) => {}
                Err(err) => tracing::warn!(error = %err, "daily maintenance failed"),
            }
        });
    }

    /// Asks `approver` before memory changes that need approval.
    #[must_use]
    pub fn with_memory_approver(mut self, approver: Arc<dyn Approver>) -> Self {
        self.approver = Some(approver);
        self
    }

    /// The launch environment.
    #[must_use]
    pub fn launch(&self) -> &LaunchEnv {
        &self.launch
    }

    /// The redactor of the profile's secrets.
    #[must_use]
    pub fn redactor(&self) -> &Redactor {
        &self.redactor
    }

    /// A new session row for this process's sessions on `target`'s route.
    ///
    /// # Errors
    ///
    /// Store errors, `session.invalid_source_tag`.
    pub fn create_session(
        &self,
        parent: Option<SessionId>,
        target: &pmt_providers::resolve::Target,
    ) -> Result<SessionId, Error> {
        let mut new = match parent {
            Some(parent) => NewSession::child(Source::Cli, parent, pmt_store::LineageKind::Fork),
            None => NewSession::new(Source::Cli),
        };
        new.provider = Some(target.provider.clone());
        new.model.clone_from(&target.model);
        if let Some(tag) = &self.options.source_tag {
            pmt_store::sessions::check_source_tag(tag)?;
            new.source_tag = Some(tag.clone());
        }
        new.cwd = Some(self.options.cwd.to_string_lossy().into_owned());
        Ok(self.store.create_session(&new)?.id)
    }

    /// Builds the running session of `id` on `target`'s route.
    ///
    /// # Errors
    ///
    /// Model, tool, store and environment errors.
    pub async fn open(
        &self,
        id: SessionId,
        target: &pmt_providers::resolve::Target,
    ) -> Result<Live, Error> {
        self.open_with(id, target, None).await
    }

    /// Like [`Factory::open`], also sending every trajectory event to
    /// `clients` (a daemon session's attached clients), which answer its
    /// secret prompts too.
    ///
    /// # Errors
    ///
    /// As [`Factory::open`].
    pub async fn open_with(
        &self,
        id: SessionId,
        target: &pmt_providers::resolve::Target,
        clients: Option<Clients>,
    ) -> Result<Live, Error> {
        self.open_inner(id, target, clients, None).await
    }

    /// Builds session `id`: a top-level session, or a delegated child
    /// narrowed by `child` (its tools, capabilities, approvals, prompt and
    /// budgets; spec: subagent-delegation, Child session isolation).
    async fn open_inner(
        &self,
        id: SessionId,
        target: &pmt_providers::resolve::Target,
        clients: Option<Clients>,
        child: Option<&delegation::ChildSpec>,
    ) -> Result<Live, Error> {
        let model = self.model_for(target, child).await?;
        self.maintain();
        let session = self.session_row(id, target)?;
        self.discover_mcp().await;
        let (registry, selection, session_surface) = self.selection_for(&session, child)?;
        let inline_shell = Arc::new(skills::DeferredShell::default());
        let skills = self.session_skills(id, &selection, &inline_shell, clients.as_ref());
        let attached = SkillSessions::process().attach(&id.to_string(), Arc::clone(&skills));
        let capabilities = self.capabilities_for(id, &skills, child)?;
        let current = SharedScope::new(self.scope_for(child));
        let environment = self.environment(
            &session,
            target,
            &capabilities,
            &current,
            &skills,
            &place::At::open(&self.options.cwd, clients.as_ref()),
        )?;
        let memory = Arc::new(MemorySession::new(self.memory_setup(id)?));
        let summarizer = self.models.summarizer(&model, id);
        let cards = shows_cards(self.options.interactive, &session_surface);
        let delegation = self.delegation_for(
            (&session, &model, &registry),
            (child, clients.as_ref()),
            &memory,
            &environment,
        );
        let (agent_tools, clarify) =
            self.agent_tools(id, &memory, &summarizer, &capabilities, cards, &delegation)?;
        let gate = self.gate_for(id, child, &model, &environment)?;
        skills::bind_shell(&inline_shell, environment.commands.as_ref(), &gate.gate);
        let ports = (&gate.gate, &agent_tools, &model);
        let built = (registry, selection, &capabilities);
        let tool_state = self.tool_state(id, built, ports).await?;
        let tool_port = tool_state.tools();
        delegation.parent_is(&tool_port, &capabilities, &gate.gate);
        let trajectory = self.trajectory(&session)?;
        let mut notices = Vec::new();
        let (prompt, plugin_sections) = self
            .prompt_for(id, (&skills, &memory), &mut notices, child)
            .await;
        let (agent, progress, mcp) = self.agent_of(opening::AgentInputs {
            id,
            target,
            session: &session,
            child,
            clients: clients.as_ref().map(|c| Arc::clone(&c.events)),
            model: &model,
            tools: &tool_port,
            tool_state: &tool_state,
            prompt,
            trajectory: trajectory.as_ref(),
            environment: &environment,
            agent_tools: &agent_tools,
            delegation: &delegation,
            summarizer,
            skills: &skills,
            memory: &memory,
            plugin_sections: &plugin_sections,
        })?;
        let (environment, egress, downgrades) = place::Environment::of(environment);
        let live = Live {
            place: place::Place::new(self.options.cwd.clone(), environment, &id.to_string()),
            clients,
            scope: self.scope_for(child),
            id,
            agent,
            _plugin: self.plugin_registrations(id, &model, delegation.delegator()),
            _mcp: mcp,
            model,
            tools: tool_port,
            gate: gate.gate,
            approvals: gate.prompts,
            clarify,
            skills,
            memory,
            skill_nudge: Mutex::new(Nudge::new(&self.scope.config().skills, session.source)),
            tool_state,
            session_surface,
            plugins: self.plugins.clone(),
            surface: session.source.as_str(),
            plugin_sections,
            notices,
            progress,
            config: current,
            record: Arc::clone(&self.record),
            capabilities,
            trajectory,
            egress,
            downgrades,
            delegation: delegation.delegator(),
            _skills: attached,
        };
        live.emit_start(&session, target).await;
        Ok(live)
    }

    /// The configuration a session runs with: the profile's, or for a
    /// delegated child the profile's with the child's budget share as its
    /// session budget (spec: subagent-delegation, Fan-out budget split).
    fn scope_for(&self, child: Option<&delegation::ChildSpec>) -> ProfileScope {
        child.map_or_else(|| self.scope.clone(), delegation::ChildSpec::scope)
    }

    /// Announces borrowed logins on `notices` and fires the API hooks
    /// around every call of the session's `model`.
    fn observe_model(&self, model: &SwitchableModel, id: SessionId, notices: progress::Notices) {
        model.watch_credentials(borrowed_watch(notices));
        model.observe_api(api_hooks::ApiHooks::new(
            self.plugins.clone(),
            self.plugin_call(id),
        ));
    }

    /// `events` with the session's lifecycle hooks fired from them; the
    /// gateway fires its own turns' lifecycle hooks.
    fn with_lifecycle_hooks(
        &self,
        id: SessionId,
        session: &pmt_store::Session,
        events: Arc<dyn EventSink>,
    ) -> Arc<dyn EventSink> {
        if session.source.as_str() == "gateway" {
            return events;
        }
        Arc::new(lifecycle::LifecycleHooks::new(
            events,
            self.plugins.clone(),
            self.plugin_call(id),
        ))
    }

    /// The registry of a session of `session`'s row, its toolset selection
    /// and the surface it starts from.
    fn session_selection(
        &self,
        session: &pmt_store::Session,
    ) -> Result<(Arc<ToolRegistry>, Selection, pmt_tools::SessionSurface), Error> {
        // Plugin and MCP toolsets: enabling a plugin or configuring a
        // server opts its tools in.
        let (registry, plugin_toolsets) = self.session_registry()?;
        let surface = tools::surface_of(session);
        let selection = tools::selection(
            &registry,
            self.scope.config(),
            self.options.interactive,
            &plugin_toolsets,
            &surface,
        )?;
        let selection = self.with_browser_opt_ins(session.id, selection);
        Ok((registry, selection, surface))
    }

    /// The parts of a session's agent setup that come from this factory's
    /// surface: redaction, interactivity and plugin turn hooks.
    fn surface_setup(&self, setup: &mut AgentSetup, id: SessionId) {
        setup.redactor = self.redactor.clone();
        setup.interactive = self.options.interactive;
        setup.unattended = !self.options.interactive;
        setup.turn_hooks = Some(self.plugins.turn_hooks(self.plugin_call(id)));
    }

    /// What plugin host calls of session `id` report as their session:
    /// the surface that created it (`cli`, `tui`, `gateway`, `cron`...)
    /// and this factory's working directory.
    fn plugin_call(&self, id: SessionId) -> pmt_plugins::host::CallInfo {
        session_call(
            id,
            &plugins::stored_surface(&self.store, id),
            &self.options.cwd,
        )
    }

    /// The session's command approval gate; with the session's `model`,
    /// `smart` mode asks the approval guardian on its auxiliary route.
    fn gate(
        &self,
        id: SessionId,
        model: Option<&Arc<SwitchableModel>>,
        environment: Option<&env::Attached>,
    ) -> Result<tools::Gate, Error> {
        let previewer = environment.and_then(|e| e.previewer.clone());
        let terminal = environment.and_then(|e| e.terminal.as_ref());
        let guardian = model.map(|model| self.models.guardian(model, id));
        let hooks = crate::plugins_host::approvals::PluginApprovals::new(
            self.plugins.clone(),
            self.plugin_call(id),
            self.redactor.clone(),
            &self.scope.config().plugins,
        );
        tools::gate(&GateInputs {
            scope: &self.scope,
            grants: &self.grants,
            cwd: &self.options.cwd,
            user_home: self.launch.user_home().map(PathBuf::as_path),
            interactive: self.options.interactive,
            yolo: self.options.yolo,
            hooks: Some(Arc::new(hooks)),
            session_id: id.to_string(),
            guardian: guardian.map(|g| g as _),
            previewer,
            hosted: self
                .daemon
                .clone()
                .map(|daemon| (daemon, own_processes(terminal))),
        })
    }

    /// The egress policy of session `id`: the profile's, with the chat
    /// flags; refusals go to the audit log and, once the session's event
    /// sink is set in `events`, to its trajectory and clients.
    fn egress(
        &self,
        id: SessionId,
        events: &Denials,
        hosts: Option<Arc<pmt_net::egress::HostPolicy>>,
    ) -> crate::egress::SessionEgress {
        let hosts = hosts.unwrap_or_else(|| {
            Arc::new(crate::egress::host_policy(
                &self.scope.config().security.egress,
                &self.options.egress,
            ))
        });
        let events = Arc::clone(events);
        let runtime = tokio::runtime::Handle::current();
        let emit: Arc<dyn Fn(&pmt_net::egress::Denial) + Send + Sync> = Arc::new(move |denial| {
            let Some(sink) = events.get().and_then(std::sync::Weak::upgrade) else {
                return;
            };
            let event = pmt_obs::trajectory::events::Event::EgressDenied(
                pmt_obs::trajectory::payload::EgressDenied {
                    host: denial.host.clone(),
                    port: denial.port,
                    requester: denial.requester.clone(),
                    reason: denial.reason.as_str().to_owned(),
                },
            );
            runtime.spawn(async move {
                let emit = pmt_obs::trajectory::writer::Emit::now(event);
                if let Err(err) = sink.emit(emit).await {
                    tracing::debug!(error = %err, "egress.denied event not recorded");
                }
            });
        });
        crate::egress::SessionEgress {
            hosts,
            observer: crate::egress::observer(&self.scope, Some(id.to_string()), Some(emit)),
        }
    }

    /// The trajectory writer of `session`, when trajectories are on.
    fn trajectory(
        &self,
        session: &pmt_store::Session,
    ) -> Result<Option<Arc<TrajectoryWriter>>, Error> {
        Ok(pmt_obs::trajectory::writer::open_for_session(
            &self.scope,
            &session.id.to_string(),
            &session.root_session_id.to_string(),
        )?
        .map(Arc::new))
    }

    /// Attaches the session's execution environment (its terminal reports
    /// finished background processes to the `clients`' sender).
    fn environment(
        &self,
        session: &pmt_store::Session,
        target: &pmt_providers::resolve::Target,
        capabilities: &CapabilitySet,
        config: &SharedScope,
        skills: &Arc<SkillsSession>,
        at: &place::At<'_>,
    ) -> Result<env::Attached, Error> {
        let id = session.id;
        let clients = at.clients;
        let denials = at.denials.clone().unwrap_or_default();
        let capabilities = &place::with_worktree_git(capabilities, at.cwd);
        let processes =
            processes::Wiring::of(self.scope.config(), &self.store, id, &session.origin);
        let environment = env::attach(&env::EnvInputs {
            scope: &self.scope,
            config,
            session_id: &id.to_string(),
            launch: &self.launch,
            cwd: at.cwd,
            capabilities,
            redactor: self.redactor.clone(),
            browser: self.browser(),
            image_input: self.image_input(target),
            completions: clients
                .and_then(|c| c.completions.clone())
                .or_else(|| self.notices.clone()),
            providers: self.plugins.current().providers.clone(),
            plugin_call: self.plugin_call(id),
            plugins: &self.plugins,
            egress: &self.egress(id, &denials, at.hosts.clone()),
            denials: &denials,
            vault_prompt: self.vault_prompt(clients),
            processes: &processes,
            snapshots: snapshots::opener(&self.scope, &self.store, id),
            input_roots: input_roots(&self.scope, skills),
            automatic_trust: automatic_trust(session),
        })?;
        // Skills pass their declared secrets to the terminal.
        if let Some(terminal) = &environment.terminal {
            terminal.set_secret_source(skills.secret_source());
            terminal.set_path_source(python::path_source(&self.scope, skills));
            terminal.set_sudo(Arc::new(sudo::handling(
                &self.scope,
                capabilities,
                &session.origin,
                self.sudo_prompt(clients),
            )));
        }
        Ok(environment)
    }

    /// Where `browser_vault` asks for a password or code: the session's
    /// clients (a daemon session) or the local terminal (an interactive
    /// chat); nobody in cron, webhook, API and single-query runs.
    fn vault_prompt(
        &self,
        clients: Option<&Clients>,
    ) -> Option<Arc<dyn pmt_tools::browser::vault::MaskedPrompt>> {
        if !self.options.interactive {
            return None;
        }
        let secrets = clients
            .map(|c| Arc::clone(&c.secrets))
            .or_else(|| self.secret_prompt.clone())?;
        Some(Arc::new(env::VaultPrompt(secrets)))
    }

    /// Where a sudo password is asked: the session's clients
    /// (`sudo.request`) or the local terminal's hidden line, on an
    /// interactive session only.
    fn sudo_prompt(
        &self,
        clients: Option<&Clients>,
    ) -> Option<Arc<dyn pmt_tools::terminal::sudo::SudoPrompt>> {
        if !self.options.interactive {
            return None;
        }
        match clients {
            Some(clients) => clients.sudo.clone(),
            None => self.secret_prompt.clone().map(|p| {
                Arc::new(sudo::HiddenLine(p)) as Arc<dyn pmt_tools::terminal::sudo::SudoPrompt>
            }),
        }
    }

    /// The baseline and granted capabilities, plus reads of the session's
    /// own tool output and of the skills' directories.
    fn capabilities(&self, id: SessionId, skills: &SkillsSession) -> Result<CapabilitySet, Error> {
        Ok(self
            .grants
            .capabilities(&Subject::Profile)?
            .union(&tool_output_reads(self.scope.dir(), &id.to_string()))
            .union(&skills.sandbox_reads())
            .union(&python::sandbox_reads(&self.scope)))
    }

    /// The loop's own tools; `clarify` asks the person on an interactive
    /// session, where `cards` also lets `manage_connections` show its
    /// connection card (else it hands out links).
    fn agent_tools(
        &self,
        id: SessionId,
        memory: &Arc<MemorySession>,
        summarizer: &Arc<summarize::RouteSummarizer>,
        capabilities: &CapabilitySet,
        cards: bool,
        delegation: &delegation::Wiring,
    ) -> Result<(Arc<AgentTools>, Option<ClarifyPrompts>), Error> {
        let (clarifier, clarify) = if self.options.interactive {
            let (clarifier, surface) =
                Clarifier::attached(self.scope.config().approvals.timeout_secs);
            (clarifier, Some(surface))
        } else {
            (Clarifier::unattended(), None)
        };
        let clarifier = clarifier.with_observer(crate::plugins_host::approvals::input_observer(
            self.plugins.clone(),
            self.plugin_call(id),
        ));
        let tools = AgentTools::new(
            Arc::clone(&self.store),
            id,
            clarifier,
            SessionSearch::new(Arc::clone(&self.store), id)
                .with_summarizer(Arc::clone(summarizer) as _),
        )?
        .with_memory(Arc::clone(memory) as _)
        .with_connections(crate::connectors_host::manager(&self.scope, cards));
        let tools = match &self.messaging {
            Some(ports) => tools.with_messaging(ports.port(id, self.read_check(capabilities))),
            None => tools,
        };
        let tools = delegation.equip(tools);
        Ok((Arc::new(tools), clarify))
    }

    /// Whether the session may read a path: its effective capabilities
    /// with the placeholders of its working directory and the user's home.
    fn read_check(&self, capabilities: &CapabilitySet) -> pmt_agent::tools::ReadCheck {
        let capabilities = capabilities.clone();
        let places = pmt_core::capability::Places {
            user_home: self.launch.user_home().cloned().unwrap_or_default(),
            workspace: self.options.cwd.clone(),
            scratch: Vec::new(),
            toolchain: Vec::new(),
        };
        Arc::new(move |path| {
            capabilities.allows_path(pmt_core::capability::Access::Read, path, &places)
        })
    }

    /// The stored row of session `id`, recorded for `--usage-file` with
    /// the launch flags; a gateway session's platform user id is hashed in
    /// every model request when PII is redacted (spec:
    /// security-boundaries, Optional PII redaction).
    fn session_row(
        &self,
        id: SessionId,
        target: &pmt_providers::resolve::Target,
    ) -> Result<pmt_store::Session, Error> {
        let session = self.store.session(id)?;
        if let pmt_store::Origin::Gateway {
            user: Some(user), ..
        } = &session.origin
        {
            self.models.register_user_id(user);
        }
        self.record.session(id, session.root_session_id, target);
        self.record_launch_flags(id)?;
        Ok(session)
    }

    /// Marks this factory's sessions as hosted by `daemon`: their gates
    /// block commands that would stop it (spec: tool-system,
    /// Self-termination guard).
    #[must_use]
    pub fn hosted_by(mut self, daemon: pmt_tools::approval::selfterm::Daemon) -> Self {
        self.daemon = Some(daemon);
        self
    }

    /// Sends `send_message` calls of this factory's sessions through
    /// `ports` (the daemon's gateway).
    #[must_use]
    pub fn with_messaging(mut self, ports: Arc<dyn pmt_agent::tools::MessagingPorts>) -> Self {
        self.messaging = Some(ports);
        self
    }

    /// The skills' view of the session: the CLI surface and its tools,
    /// snippets run in the session sandbox, and on an interactive session
    /// missing secrets asked for locally (or by the session's `clients`).
    fn skills_setup(
        &self,
        id: SessionId,
        selection: &Selection,
        inline_shell: &Arc<skills::DeferredShell>,
        clients: Option<&Clients>,
    ) -> SessionSetup {
        let mut setup =
            SessionSetup::new(self.scope.clone(), Arc::clone(&self.store), id.to_string());
        setup.cwd = Some(self.options.cwd.clone());
        setup.user_home = self.launch.user_home().cloned();
        setup.view = skills::view(selection);
        setup.plugins = self.plugins.current().skill_dirs();
        let (plugins, call) = (self.plugins.clone(), self.plugin_call(id));
        setup.lifecycle = Some(Arc::new(move |skill: &str, action: &str| {
            plugins.observe(
                call.clone(),
                "on_skill_lifecycle",
                crate::plugins_host::skill_fields(skill, action),
            );
        }));
        setup.inline_shell = Some(Arc::clone(inline_shell) as _);
        setup.secret_prompt = clients
            .map(|c| Arc::clone(&c.secrets))
            .or_else(|| self.secret_prompt.clone());
        setup
    }

    fn memory_setup(&self, id: SessionId) -> Result<MemorySetup, Error> {
        let mut setup = MemorySetup::new(
            self.scope.clone(),
            Arc::clone(&self.store),
            Some(id),
            Source::Cli,
        );
        setup.redactor = self.redactor.clone();
        setup.approver.clone_from(&self.approver);
        let prior = self
            .store
            .messages(id, pmt_store::Visibility::Active)?
            .iter()
            .filter(|m| m.role == pmt_store::Role::User)
            .count();
        setup.prior_user_turns = u32::try_from(prior).unwrap_or(u32::MAX);
        Ok(setup)
    }

    /// The session's tools, built from `registry` and `selection` for the
    /// route `model` is on, and the state that rebuilds them.
    ///
    /// # Errors
    ///
    /// Store errors recording the surface.
    async fn tool_state(
        &self,
        id: SessionId,
        (registry, selection, capabilities): (Arc<ToolRegistry>, Selection, &CapabilitySet),
        (gate, agent, model): (&Arc<CommandGate>, &Arc<AgentTools>, &Arc<SwitchableModel>),
    ) -> Result<Arc<ToolState>, Error> {
        let route = tools::route_traits(model.as_ref()).await;
        let inputs = tool_state::Inputs {
            scope: self.scope.clone(),
            store: Arc::clone(&self.store),
            session: id,
            capabilities: capabilities.clone(),
            gate: Arc::clone(gate),
            agent: Arc::clone(agent),
            redactor: self.redactor.clone(),
            hooks: self
                .plugins
                .tool_hooks_with_guidance(self.plugin_call(id), &self.scope.config().plugins),
            model: Arc::clone(model),
        };
        let built = tools::session_tools(&inputs.surface(&registry, &selection, &route)).await?;
        Ok(Arc::new(ToolState::new(
            Arc::clone(&self.registries),
            inputs,
            Arc::new(SwappableTools::new(built)),
            (registry, selection, route),
        )))
    }

    /// The session's prompt inputs and its plugin sections (kept for
    /// rebuilds; `/plugins reload --now` replaces them).
    async fn session_prompt(
        &self,
        id: SessionId,
        skills: &Arc<SkillsSession>,
        memory: &Arc<MemorySession>,
        notices: &mut Vec<String>,
    ) -> (PromptSetup, Arc<Mutex<Vec<String>>>) {
        let mut prompt = self.prompt_setup(skills, memory, notices).await;
        prompt.plugin_sections = self.plugin_sections(id).await;
        let sections = Arc::new(Mutex::new(prompt.plugin_sections.clone()));
        (prompt, sections)
    }

    /// Sets the session's summarizer, memory and prompt refresher on
    /// `setup`.
    fn memory_and_refresh(
        &self,
        setup: &mut AgentSetup,
        summarizer: Arc<summarize::RouteSummarizer>,
        skills: &Arc<SkillsSession>,
        memory: &Arc<MemorySession>,
        sections: &Arc<Mutex<Vec<String>>>,
    ) {
        setup.summarizer = Some(summarizer as _);
        setup.memory = Some(Arc::clone(memory) as _);
        setup.refresh_prompt = Some(refresher(
            skills,
            memory,
            sections,
            self.options.ignore_rules,
        ));
    }

    async fn prompt_setup(
        &self,
        skills: &Arc<SkillsSession>,
        memory: &Arc<MemorySession>,
        notices: &mut Vec<String>,
    ) -> PromptSetup {
        let mut prompt = PromptSetup::new("cli", Some(self.options.cwd.clone()));
        prompt.no_context_files = self.options.no_context_files;
        prompt.pass_session_id = self.options.pass_session_id;
        prompt.started_at = Timestamp::now();
        prompt.skills_index = skills.index_text();
        if self.options.ignore_rules {
            prompt.no_context_files = true;
            return prompt;
        }
        prompt.memory = memory.snapshot();
        let preloaded = pmt_skills::preload::preload(skills, &self.options.skills).await;
        notices.extend(preloaded.not_found_lines());
        prompt.pinned_skills = preloaded.text;
        prompt
    }

    /// Records `--ignore-user-config` / `--ignore-rules` on `id`.
    fn record_launch_flags(&self, id: SessionId) -> Result<(), Error> {
        use pmt_store::onboarding::LaunchFlag;
        let flags: Vec<LaunchFlag> = [
            (
                self.options.ignore_user_config,
                LaunchFlag::IgnoreUserConfig,
            ),
            (self.options.ignore_rules, LaunchFlag::IgnoreRules),
        ]
        .into_iter()
        .filter_map(|(on, flag)| on.then_some(flag))
        .collect();
        if flags.is_empty() {
            return Ok(());
        }
        self.store.set_launch_flags(id, &flags)
    }

    /// Rebuilds `live`'s tool surface from `selection` (`/tools ... --now`);
    /// the new tools serve from the next turn. Schema variants are picked
    /// again for the route the session is on now.
    ///
    /// # Errors
    ///
    /// Store errors recording the surface.
    pub(crate) async fn rebuild_tools(
        &self,
        live: &Live,
        selection: &Selection,
    ) -> Result<(), Error> {
        live.tool_state
            .rebuild(live.registry(), selection.clone())
            .await
    }
}

impl Factory {
    /// The tools of `live`'s background skill review running in `child`:
    /// the skills toolset only, under the session's capabilities and gate
    /// (spec: skills, Autonomous skill creation and improvement).
    ///
    /// # Errors
    ///
    /// The skills toolset is not registered; store errors recording the
    /// surface.
    pub(crate) async fn review_tools(
        &self,
        live: &Live,
        child: SessionId,
    ) -> Result<Arc<SessionTools>, Error> {
        let registry = live.registry();
        let route = live.tool_state.route();
        let selection =
            Selection::resolve(&registry, &[pmt_skills::tools::TOOLSET.to_owned()], &[])?;
        tools::session_tools(&SurfaceInputsOwned {
            scope: &self.scope,
            store: &self.store,
            session: child,
            registry: &registry,
            selection: &selection,
            capabilities: &live.capabilities,
            gate: &live.gate,
            agent: None,
            redactor: &self.redactor,
            context_window: None,
            hooks: self
                .plugins
                .tool_hooks_with_guidance(self.plugin_call(child), &self.scope.config().plugins),
            route: &route,
        })
        .await
    }
}

/// Whether a session shows connection cards: an interactive terminal,
/// the TUI or the desktop (messaging chats, one-shot runs and cron get
/// links instead).
fn shows_cards(interactive: bool, surface: &pmt_tools::SessionSurface) -> bool {
    use pmt_tools::SessionSurface::{Cli, Desktop, Tui};
    interactive && matches!(surface, Cli | Tui | Desktop)
}

/// Announces each borrowed login's first use in the session (spec:
/// model-providers, Borrowed CLI logins).
/// Where a session's events go: its trajectory (when on) and its daemon
/// clients (when attached).
fn events_to(
    trajectory: Option<Arc<TrajectoryWriter>>,
    clients: Option<Arc<dyn EventSink>>,
) -> Arc<dyn EventSink> {
    let events = trajectory.map_or_else(|| Arc::new(NoEvents) as Arc<dyn EventSink>, |t| t as _);
    match clients {
        Some(clients) => Arc::new(progress::Both(events, clients)),
        None => events,
    }
}

fn borrowed_watch(notices: progress::Notices) -> pmt_agent::turn::model::CredentialWatch {
    let announced = Mutex::new(pmt_providers::auth::borrowed::Announcements::default());
    Arc::new(move |credential| {
        let notice = announced
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .first_use(credential);
        if let Some(notice) = notice {
            notices.show(notice);
        }
    })
}

/// Re-reads the skills index and the memory snapshot before every prompt
/// rebuild (a `--now` change or a compaction), never in between; the
/// plugin sections are the session's own (changed only by `/plugins
/// reload --now`).
fn refresher(
    skills: &Arc<SkillsSession>,
    memory: &Arc<MemorySession>,
    plugin_sections: &Arc<Mutex<Vec<String>>>,
    ignore_rules: bool,
) -> RefreshPrompt {
    let skills = Arc::clone(skills);
    let memory = Arc::clone(memory);
    let sections = Arc::clone(plugin_sections);
    Arc::new(move |prompt: &mut PromptSetup| {
        prompt.skills_index = skills.index_text();
        prompt.plugin_sections.clone_from(&plugins::lock(&sections));
        if !ignore_rules {
            memory.refresh(prompt);
        }
    })
}

/// The `session.info` of a plugin call made for session `id` from
/// `surface` in `cwd`.
/// Whether a session's working directory earns language-server trust
/// automatically: not for scheduled jobs or kanban workers (spec:
/// lsp-diagnostics, Workspace trust).
fn automatic_trust(session: &pmt_store::Session) -> bool {
    session.source != pmt_store::Source::Cron
        && !session
            .source_tag
            .as_deref()
            .is_some_and(|tag| tag.starts_with(KANBAN_SOURCE_TAG))
}

/// The source tag kanban worker sessions carry.
pub const KANBAN_SOURCE_TAG: &str = "kanban";

fn session_call(id: SessionId, surface: &str, cwd: &Path) -> pmt_plugins::host::CallInfo {
    pmt_plugins::host::CallInfo {
        session_id: Some(id.to_string()),
        surface: surface.to_owned(),
        cwd: Some(cwd.to_string_lossy().into_owned()),
    }
}

impl Live {
    /// What a plugin call made for this session from `surface` (where the
    /// user typed it: `cli`, `tui`, `gateway`...) reports as its session.
    #[must_use]
    pub fn plugin_call(&self, surface: &str) -> pmt_plugins::host::CallInfo {
        session_call(self.id, surface, &self.place.cwd())
    }

    /// The surface that created the session (`cli`, `tui`, `gateway`...).
    #[must_use]
    pub fn surface(&self) -> &'static str {
        self.surface
    }

    /// The tool registry the session's tools come from.
    #[must_use]
    pub fn registry(&self) -> Arc<ToolRegistry> {
        self.tool_state.registry()
    }

    /// The toolset selection the session's tools are built from.
    #[must_use]
    pub fn selection(&self) -> Selection {
        self.tool_state.selection()
    }

    /// Records `session.start` for `session` on `target`'s route, then
    /// one `security.downgrade` per allowed hardening downgrade.
    async fn emit_start(
        &self,
        session: &pmt_store::Session,
        target: &pmt_providers::resolve::Target,
    ) {
        self.emit(Event::SessionStart(SessionStart {
            source: session.source.as_str().to_owned(),
            title: session.title.clone(),
            model: target.model.clone(),
            parent_session_id: session.parent_session_id.map(|p| p.to_string()),
        }))
        .await;
        for event in self.downgrades.events() {
            self.emit(event).await;
        }
        // The gateway fires its own sessions' start through its hooks.
        if self.surface() != "gateway" {
            let fields = lifecycle::session_start(&self.id.to_string(), self.surface());
            self.plugins
                .fire_observer(
                    &self.plugin_call(self.surface()),
                    "on_session_start",
                    fields,
                )
                .await;
        }
    }

    /// Emits `event` outside any turn.
    pub async fn emit(&self, event: Event) {
        let Some(writer) = &self.trajectory else {
            return;
        };
        let emit = Emit {
            ts: Timestamp::now(),
            turn_id: None,
            span_id: pmt_obs::trajectory::new_span_id(),
            parent_span_id: None,
            event,
        };
        if let Err(err) = writer.emit(emit).await {
            tracing::debug!(error = %err, "trajectory event dropped");
        }
    }

    /// Ends the session row with `reason`, records `session.end` and
    /// closes the trajectory.
    pub async fn close(self, store: &Store, reason: EndReason) {
        self.end(store, reason).await;
        // Webhooks of the session's last events go out before the process
        // that ran it may exit.
        crate::hooks_host::outbound::flush(crate::hooks_host::outbound::FLUSH).await;
        // Everything else the session holds (its agent, tools, delegation)
        // may reach the writer through its event sinks: it is dropped
        // first so the writer can be finished.
        let trajectory = self.trajectory.clone();
        drop(self);
        if let Some(writer) = trajectory.and_then(|t| Arc::try_unwrap(t).ok())
            && let Err(err) = writer.finish()
        {
            tracing::warn!(error = %err, "could not close the trajectory");
        }
    }

    /// Unloads the session without ending it (the messaging gateway's
    /// agent cache): the record stays open and its next turn rebuilds the
    /// agent from the store with the same frozen prompt. Built-in memory
    /// is written to the session's own profile store at every call, so
    /// nothing is left to flush.
    pub async fn unload(self) {
        if let Some(browser) = &self.place.browser() {
            browser.close_session(&self.id.to_string()).await;
        }
        // Everything else the session holds (its agent, tools, delegation)
        // may reach the writer through its event sinks: it is dropped
        // first so the writer can be finished.
        let trajectory = self.trajectory.clone();
        drop(self);
        if let Some(writer) = trajectory.and_then(|t| Arc::try_unwrap(t).ok())
            && let Err(err) = writer.finish()
        {
            tracing::warn!(error = %err, "could not close the trajectory");
        }
    }

    /// Ends the session row with `reason` and records `session.end`,
    /// leaving the trajectory file open: for a session something else
    /// still holds when the process stops (prefer [`Live::close`]).
    pub async fn end(&self, store: &Store, reason: EndReason) {
        if let Some(delegation) = &self.delegation {
            delegation.close();
        }
        self.emit(Event::SessionEnd(SessionEnd {
            reason: reason.as_str().to_owned(),
        }))
        .await;
        if let Err(err) = store.end_session(self.id, reason) {
            tracing::warn!(error = %err, session = %self.id, "could not end the session");
        }
        // The gateway fires its own sessions' end through its hooks.
        if self.surface() != "gateway" {
            let fields = serde_json::json!({
                "session_id": self.id.to_string(),
                "reason": reason.as_str(),
                "platform": self.surface(),
            });
            if let serde_json::Value::Object(fields) = fields {
                self.plugins
                    .fire_observer(&self.plugin_call(self.surface()), "on_session_end", fields)
                    .await;
            }
        }
        if let Some(browser) = &self.place.browser() {
            browser.close_session(&self.id.to_string()).await;
        }
        if let Some(text) = self.sync_back().await {
            tracing::info!(session = %self.id, "{text}");
        }
        self.place.release();
    }

    /// Copies the remote workspace's changes back to the host (session end
    /// and `/sync`); `None` without sync-back.
    pub async fn sync_back(&self) -> Option<String> {
        let sync = self.place.sync()?;
        let budget = self.config.get().config().terminal.sync_back_max_bytes;
        let done = tokio::task::spawn_blocking(move || sync.sync()).await;
        Some(match done {
            Ok(Ok(report)) => {
                let copied = format!("sync-back copied {} files", report.copied.len());
                match report.notice(budget) {
                    Some(notice) => format!("{copied}; {notice}"),
                    None => copied,
                }
            }
            Ok(Err(why)) => format!("sync-back failed: {why}"),
            Err(err) => format!("sync-back failed: {err}"),
        })
    }
}

/// The running background processes of a session's terminal (none when
/// the session has no local terminal).
fn own_processes(
    terminal: Option<&Arc<pmt_tools::terminal::Terminal>>,
) -> pmt_tools::approval::OwnProcesses {
    let terminal = terminal.map(Arc::downgrade);
    Arc::new(move || {
        terminal
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
            .map(|t| t.processes().running_pids())
            .unwrap_or_default()
    })
}

/// Host directories a remote command may name: the profile's received
/// attachments (`cache/media/`) and every skill directory on disk.
fn input_roots(scope: &ProfileScope, skills: &SkillsSession) -> Vec<std::path::PathBuf> {
    let mut roots = vec![scope.dir().join("cache").join("media"), skills.skills_dir()];
    roots.extend(skills.disk_roots());
    roots.dedup();
    roots
}

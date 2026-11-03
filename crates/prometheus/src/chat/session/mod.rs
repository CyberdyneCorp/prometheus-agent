//! Session start for the CLI (spec: cli, agent-turn-loop, tool-system):
//! everything one chat session runs with, built once per session and torn
//! down when it ends. [`Factory`] holds what every session of the process
//! shares (profile, store, model side, tool registry, grants);
//! [`Factory::open`] builds one [`Live`] session over a new or resumed
//! session row.

pub mod browser;
pub mod env;
pub mod model;
mod plugins;
pub mod progress;
pub mod skills;
pub mod summarize;
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

use self::model::{Models, RouteFlags, SwitchableModel};
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
}

/// The clients of one daemon session: they see its events and answer its
/// secret prompts (a factory serves several sessions, each with its own).
pub struct Clients {
    /// Where the session's trajectory events also go.
    pub events: Arc<dyn EventSink>,
    /// Asks the session's clients for a skill's missing secret.
    pub secrets: Arc<dyn SecretPrompt>,
    /// Where the terminal reports finished `notify_on_complete` background
    /// processes (a gateway session's chat).
    pub completions:
        Option<tokio::sync::mpsc::UnboundedSender<pmt_tools::terminal::process::Completion>>,
}

/// What every session of this process shares.
pub struct Factory {
    /// The profile.
    pub scope: ProfileScope,
    /// The profile's store.
    pub store: Arc<Store>,
    /// The model side.
    pub models: Models,
    /// The core tools plus those of the plugin set they were built for;
    /// read through [`Factory::session_registry`].
    tools: Mutex<plugins::PluginTools>,
    grants: Arc<SessionGrants>,
    launch: LaunchEnv,
    redactor: Redactor,
    /// What this run did (`--usage-file`).
    pub record: Arc<crate::chat::usage_file::RunRecord>,
    /// The flags.
    pub options: Options,
    approver: Option<Arc<dyn Approver>>,
    secret_prompt: Option<Arc<dyn SecretPrompt>>,
    maintained: std::sync::atomic::AtomicBool,
    install_asks: Option<tokio::sync::mpsc::UnboundedSender<browser::InstallAsk>>,
    /// The browser tools every session shares, set up by the first
    /// session (`None`: they could not be, see the WARN).
    browser: OnceLock<Option<browser::Shared>>,
    /// The gateway's `send_message` ports (daemon sessions only).
    messaging: Option<Arc<dyn pmt_agent::tools::MessagingPorts>>,
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
}

/// One running session.
pub struct Live {
    /// The profile.
    pub scope: ProfileScope,
    /// The session.
    pub id: SessionId,
    /// The turn loop.
    pub agent: Agent,
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
    /// The toolset selection the session's tools are built from (`/tools
    /// --now` replaces it).
    selection: Mutex<Selection>,
    /// The tool registry the session's tools come from (`/plugins reload
    /// --now` replaces it).
    registry: Mutex<Arc<ToolRegistry>>,
    /// The profile's live plugin set (slash commands, reload).
    pub plugins: crate::plugins_host::ProfilePlugins,
    /// The session's working directory (plugin `session.info`).
    cwd: PathBuf,
    /// The surface that created the session (`cli`, `tui`, `gateway`...).
    surface: &'static str,
    /// The plugin system-prompt sections the session's prompt carries.
    plugin_sections: Arc<Mutex<Vec<String>>>,
    _plugin_llm: crate::plugins_host::llm::Registration,
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
    agent_tools: Arc<AgentTools>,
    capabilities: CapabilitySet,
    trajectory: Option<Arc<TrajectoryWriter>>,
    browser: Option<Arc<pmt_tools::browser::BrowserExec>>,
    _environment: pmt_tools::environments::Attachment,
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
        let config = scope.config();
        // Settings only a session acts on fail here, before any turn
        // (spec: execution-environments, Typed backend registry and
        // selection; memory, Context engine selection).
        pmt_sandbox::backend::BackendRegistry::builtin().select(&config.terminal.backend)?;
        pmt_agent::compress::engine::select(&config.context, &[])?;
        let store = Arc::new(Store::open(&scope)?);
        let user_home = launch.user_home().map(PathBuf::as_path);
        let models = Models::open(&scope, &store, user_home, flags, ad_hoc)?;
        let plugins = crate::plugins_host::for_session(&scope, &options.cwd);
        let tools = Mutex::new(plugins::PluginTools::build(
            &scope,
            &plugins,
            &plugins::session_calls(&store, &options.cwd),
        )?);
        let grants = Arc::new(SessionGrants::new(
            baseline(&scope.config().security),
            GrantsFile::of(scope.profile()),
        ));
        let redactor = scope.secrets().redactor().unwrap_or_default();
        Ok(Self {
            current: SharedScope::new(scope.clone()),
            plugin_scope: SharedScope::new(scope.clone()),
            scope,
            store,
            models,
            tools,
            grants,
            launch: launch.clone(),
            redactor,
            record: Arc::default(),
            options,
            approver: None,
            secret_prompt: None,
            maintained: std::sync::atomic::AtomicBool::new(false),
            install_asks: None,
            browser: OnceLock::new(),
            messaging: None,
            plugins,
            reread: None,
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
    fn browser(&self) -> Option<&browser::Shared> {
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

    /// Whether the model of `target` accepts images.
    fn image_input(&self, target: &pmt_providers::resolve::Target) -> bool {
        target.model.as_deref().is_some_and(|model| {
            self.models
                .catalog()
                .metadata(&target.provider, model)
                .0
                .supports_vision
        })
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
        let scope = &self.scope;
        let config = scope.config();
        let interactive = self.options.interactive;
        let model = Arc::new(SwitchableModel::new(
            target.clone(),
            self.models.port(target).await?,
        ));
        self.maintain();
        let session = self.store.session(id)?;
        self.record.session(id, session.root_session_id, target);
        self.record_launch_flags(id)?;
        let (registry, plugin_toolsets) = self.session_registry()?;
        let selection = tools::selection(&registry, config, interactive, &plugin_toolsets)?;
        let inline_shell = Arc::new(skills::DeferredShell::default());
        let skills = Arc::new(SkillsSession::new(self.skills_setup(
            id,
            &selection,
            &inline_shell,
            clients.as_ref(),
        )));
        let attached = SkillSessions::process().attach(&id.to_string(), Arc::clone(&skills));
        let capabilities = self.capabilities(id, &skills)?;
        let current = SharedScope::new(scope.clone());
        let environment = self.environment(
            id,
            target,
            &capabilities,
            &current,
            &skills,
            clients.as_ref(),
        )?;
        let memory = Arc::new(MemorySession::new(self.memory_setup(id)?));
        let summarizer = self.models.summarizer(&model, id);
        let (agent_tools, clarify) = self.agent_tools(id, &memory, &summarizer, &capabilities)?;
        let gate = self.gate()?;
        skills::bind_shell(&inline_shell, environment.commands.as_ref(), &gate.gate);
        let built = (&*registry, &selection, &capabilities);
        let tool_port = self.tool_port(id, built, &gate.gate, &agent_tools).await?;
        let trajectory = self.trajectory(&session)?;
        let mut notices = Vec::new();
        let (prompt, plugin_sections) = self
            .session_prompt(id, &skills, &memory, &mut notices)
            .await;
        let mut setup = AgentSetup::new(
            scope.clone(),
            Arc::clone(&self.store),
            id,
            Arc::clone(&model) as _,
            Arc::clone(&tool_port) as _,
            prompt,
        );
        let events = events_to(trajectory.clone(), clients.map(|c| c.events));
        let (tee, progress) = progress::Tee::new(events, interactive);
        model.watch_credentials(borrowed_watch(tee.notices()));
        setup.events = Arc::new(tee);
        setup.summarizer = Some(summarizer as _);
        setup.memory = Some(Arc::clone(&memory) as _);
        setup.refresh_prompt = Some(self.refresher(&skills, &memory, &plugin_sections));
        self.surface_setup(&mut setup, id);
        let agent = Agent::new(setup)?;
        let live = Live {
            scope: scope.clone(),
            id,
            agent,
            _plugin_llm: self.plugin_llm(id, &model),
            model,
            tools: tool_port,
            gate: gate.gate,
            approvals: gate.prompts,
            clarify,
            skills,
            memory,
            skill_nudge: Mutex::new(Nudge::new(&config.skills, session.source)),
            selection: Mutex::new(selection),
            registry: Mutex::new(registry),
            plugins: self.plugins.clone(),
            cwd: self.options.cwd.clone(),
            surface: session.source.as_str(),
            plugin_sections,
            notices,
            progress,
            config: current,
            record: Arc::clone(&self.record),
            agent_tools,
            capabilities,
            trajectory,
            browser: environment.browser,
            _environment: environment.attachment,
            _skills: attached,
        };
        live.emit_start(&session, target).await;
        Ok(live)
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

    /// The session's command approval gate.
    fn gate(&self) -> Result<tools::Gate, Error> {
        tools::gate(&GateInputs {
            scope: &self.scope,
            grants: &self.grants,
            cwd: &self.options.cwd,
            user_home: self.launch.user_home().map(PathBuf::as_path),
            interactive: self.options.interactive,
            yolo: self.options.yolo,
        })
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
        id: SessionId,
        target: &pmt_providers::resolve::Target,
        capabilities: &CapabilitySet,
        config: &SharedScope,
        skills: &Arc<SkillsSession>,
        clients: Option<&Clients>,
    ) -> Result<env::Attached, Error> {
        let environment = env::attach(&env::EnvInputs {
            scope: &self.scope,
            config,
            session_id: &id.to_string(),
            launch: &self.launch,
            cwd: &self.options.cwd,
            capabilities,
            redactor: self.redactor.clone(),
            browser: self.browser(),
            image_input: self.image_input(target),
            completions: clients.and_then(|c| c.completions.clone()),
            providers: self.plugins.current().providers.clone(),
            plugin_call: self.plugin_call(id),
        })?;
        // Skills pass their declared secrets to the terminal.
        if let Some(terminal) = &environment.terminal {
            terminal.set_secret_source(skills.secret_source());
        }
        Ok(environment)
    }

    /// The baseline and granted capabilities, plus reads of the session's
    /// own tool output and of the skills' directories.
    fn capabilities(&self, id: SessionId, skills: &SkillsSession) -> Result<CapabilitySet, Error> {
        Ok(self
            .grants
            .capabilities(&Subject::Profile)?
            .union(&tool_output_reads(self.scope.dir(), &id.to_string()))
            .union(&skills.sandbox_reads()))
    }

    /// The loop's own tools; `clarify` asks the person on an interactive
    /// session.
    fn agent_tools(
        &self,
        id: SessionId,
        memory: &Arc<MemorySession>,
        summarizer: &Arc<summarize::RouteSummarizer>,
        capabilities: &CapabilitySet,
    ) -> Result<(Arc<AgentTools>, Option<ClarifyPrompts>), Error> {
        let (clarifier, clarify) = if self.options.interactive {
            let (clarifier, surface) =
                Clarifier::attached(self.scope.config().approvals.timeout_secs);
            (clarifier, Some(surface))
        } else {
            (Clarifier::unattended(), None)
        };
        let tools = AgentTools::new(
            Arc::clone(&self.store),
            id,
            clarifier,
            SessionSearch::new(Arc::clone(&self.store), id)
                .with_summarizer(Arc::clone(summarizer) as _),
        )?
        .with_memory(Arc::clone(memory) as _);
        let tools = match &self.messaging {
            Some(ports) => tools.with_messaging(ports.port(id, self.read_check(capabilities))),
            None => tools,
        };
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

    /// The session's tool port over `registry` and `selection`.
    async fn tool_port(
        &self,
        id: SessionId,
        (registry, selection, capabilities): (&ToolRegistry, &Selection, &CapabilitySet),
        gate: &Arc<CommandGate>,
        agent: &Arc<AgentTools>,
    ) -> Result<Arc<SwappableTools>, Error> {
        let session_tools = tools::session_tools(&SurfaceInputsOwned {
            scope: &self.scope,
            store: &self.store,
            session: id,
            registry,
            selection,
            capabilities,
            gate,
            agent: Some(agent),
            redactor: &self.redactor,
            context_window: None,
            hooks: self.plugins.tool_hooks(self.plugin_call(id)),
        })
        .await?;
        Ok(Arc::new(SwappableTools::new(session_tools)))
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

    /// The session's prompt refresher.
    fn refresher(
        &self,
        skills: &Arc<SkillsSession>,
        memory: &Arc<MemorySession>,
        sections: &Arc<Mutex<Vec<String>>>,
    ) -> RefreshPrompt {
        refresher(skills, memory, sections, self.options.ignore_rules)
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
    /// the new tools serve from the next turn.
    ///
    /// # Errors
    ///
    /// Store errors recording the surface.
    pub(crate) async fn rebuild_tools(
        &self,
        live: &Live,
        selection: &Selection,
    ) -> Result<(), Error> {
        let registry = live.registry();
        let tools = tools::session_tools(&SurfaceInputsOwned {
            scope: &self.scope,
            store: &self.store,
            session: live.id,
            registry: &registry,
            selection,
            capabilities: &live.capabilities,
            gate: &live.gate,
            agent: Some(&live.agent_tools),
            redactor: &self.redactor,
            context_window: None,
            hooks: self.plugins.tool_hooks(self.plugin_call(live.id)),
        })
        .await?;
        live.tools.swap(tools);
        Ok(())
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
            hooks: self.plugins.tool_hooks(self.plugin_call(child)),
        })
        .await
    }
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
        session_call(self.id, surface, &self.cwd)
    }

    /// The surface that created the session (`cli`, `tui`, `gateway`...).
    #[must_use]
    pub fn surface(&self) -> &'static str {
        self.surface
    }

    /// The tool registry the session's tools come from.
    #[must_use]
    pub fn registry(&self) -> Arc<ToolRegistry> {
        Arc::clone(&plugins::lock(&self.registry))
    }

    /// The toolset selection the session's tools are built from.
    #[must_use]
    pub fn selection(&self) -> Selection {
        self.selection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Records the selection the session's tools were rebuilt from.
    pub fn set_selection(&self, selection: Selection) {
        *self
            .selection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = selection;
    }

    /// Records `session.start` for `session` on `target`'s route.
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
        let trajectory = self.trajectory;
        drop(self.agent);
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
        if let Some(browser) = &self.browser {
            browser.close_session(&self.id.to_string()).await;
        }
        let trajectory = self.trajectory;
        drop(self.agent);
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
        self.emit(Event::SessionEnd(SessionEnd {
            reason: reason.as_str().to_owned(),
        }))
        .await;
        if let Err(err) = store.end_session(self.id, reason) {
            tracing::warn!(error = %err, session = %self.id, "could not end the session");
        }
        if let Some(browser) = &self.browser {
            browser.close_session(&self.id.to_string()).await;
        }
    }
}

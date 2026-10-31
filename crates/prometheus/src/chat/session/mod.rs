//! Session start for the CLI (spec: cli, agent-turn-loop, tool-system):
//! everything one chat session runs with, built once per session and torn
//! down when it ends. [`Factory`] holds what every session of the process
//! shares (profile, store, model side, tool registry, grants);
//! [`Factory::open`] builds one [`Live`] session over a new or resumed
//! session row.

pub mod env;
pub mod model;
pub mod tools;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use jiff::Timestamp;
use pmt_agent::tools::clarify::{Clarifier, ClarifyAnswer, ClarifyRequest};
use pmt_agent::tools::{AgentTools, SessionSearch};
use pmt_agent::turn::{
    Agent, AgentSetup, EventSink, NoEvents, PromptSetup, RefreshPrompt, SessionTools,
};
use pmt_config::ProfileScope;
use pmt_config::grants::{GrantsFile, SessionGrants, Subject, baseline};
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
}

/// What every session of this process shares.
pub struct Factory {
    /// The profile.
    pub scope: ProfileScope,
    /// The profile's store.
    pub store: Arc<Store>,
    /// The model side.
    pub models: Models,
    /// The tool registry.
    pub registry: Arc<ToolRegistry>,
    grants: Arc<SessionGrants>,
    launch: LaunchEnv,
    redactor: Redactor,
    /// The flags.
    pub options: Options,
    approver: Option<Arc<dyn Approver>>,
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
    /// The toolset selection the surface was built from.
    pub selection: Selection,
    /// Notices to show once at start.
    pub notices: Vec<String>,
    agent_tools: Arc<AgentTools>,
    capabilities: CapabilitySet,
    trajectory: Option<Arc<TrajectoryWriter>>,
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
        let store = Arc::new(Store::open(&scope)?);
        let user_home = launch.user_home().map(PathBuf::as_path);
        let models = Models::open(&scope, &store, user_home, flags, ad_hoc)?;
        let registry = Arc::new(crate::tools_cmd::registry(scope.name().as_str())?);
        let grants = Arc::new(SessionGrants::new(
            baseline(&scope.config().security),
            GrantsFile::of(scope.profile()),
        ));
        let redactor = scope.secrets().redactor().unwrap_or_default();
        Ok(Self {
            scope,
            store,
            models,
            registry,
            grants,
            launch: launch.clone(),
            redactor,
            options,
            approver: None,
        })
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

    /// A new session row for this process's sessions.
    ///
    /// # Errors
    ///
    /// Store errors, `session.invalid_source_tag`.
    pub fn create_session(&self, parent: Option<SessionId>) -> Result<SessionId, Error> {
        let mut new = match parent {
            Some(parent) => NewSession::child(Source::Cli, parent, pmt_store::LineageKind::Fork),
            None => NewSession::new(Source::Cli),
        };
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
        let scope = &self.scope;
        let config = scope.config();
        let interactive = self.options.interactive;
        let user_home = self.launch.user_home().map(PathBuf::as_path);
        let model = Arc::new(SwitchableModel::new(
            target.clone(),
            self.models.port(target).await?,
        ));
        let session = self.store.session(id)?;
        let skills = Arc::new(SkillsSession::new(self.skills_setup(id)));
        let attached = SkillSessions::process().attach(&id.to_string(), Arc::clone(&skills));
        let capabilities = self.capabilities(id, &skills)?;
        let environment = env::attach(&env::EnvInputs {
            scope,
            session_id: &id.to_string(),
            launch: &self.launch,
            cwd: &self.options.cwd,
            capabilities: &capabilities,
            redactor: self.redactor.clone(),
        })?;
        let memory = Arc::new(MemorySession::new(self.memory_setup(id)?));
        let (agent_tools, clarify) = self.agent_tools(id, &memory)?;
        let gate = tools::gate(&GateInputs {
            scope,
            grants: &self.grants,
            cwd: &self.options.cwd,
            user_home,
            interactive,
            yolo: self.options.yolo,
        })?;
        let selection = tools::selection(&self.registry, config, interactive)?;
        let session_tools = tools::session_tools(&SurfaceInputsOwned {
            scope,
            store: &self.store,
            session: id,
            registry: &self.registry,
            selection: &selection,
            capabilities: &capabilities,
            gate: &gate.gate,
            agent: Some(&agent_tools),
            redactor: &self.redactor,
            context_window: None,
        })
        .await?;
        let tool_port = Arc::new(SwappableTools::new(session_tools));
        let trajectory = pmt_obs::trajectory::writer::open_for_session(
            scope,
            &id.to_string(),
            &session.root_session_id.to_string(),
        )?
        .map(Arc::new);
        let mut notices = Vec::new();
        let prompt = self.prompt_setup(&skills, &memory, &mut notices).await;
        let mut setup = AgentSetup::new(
            scope.clone(),
            Arc::clone(&self.store),
            id,
            Arc::clone(&model) as _,
            Arc::clone(&tool_port) as _,
            prompt,
        );
        setup.events = trajectory
            .clone()
            .map_or_else(|| Arc::new(NoEvents) as Arc<dyn EventSink>, |t| t as _);
        setup.redactor = self.redactor.clone();
        setup.memory = Some(Arc::clone(&memory) as _);
        setup.interactive = interactive;
        setup.unattended = !interactive;
        setup.refresh_prompt = Some(refresher(&skills, &memory));
        let agent = Agent::new(setup)?;
        let live = Live {
            scope: scope.clone(),
            id,
            agent,
            model,
            tools: tool_port,
            gate: gate.gate,
            approvals: gate.prompts,
            clarify,
            skills,
            memory,
            skill_nudge: Mutex::new(Nudge::new(&config.skills, session.source)),
            selection,
            notices,
            agent_tools,
            capabilities,
            trajectory,
            _environment: environment,
            _skills: attached,
        };
        live.emit_start(&session, target).await;
        Ok(live)
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
            SessionSearch::new(Arc::clone(&self.store), id),
        )?
        .with_memory(Arc::clone(memory) as _);
        Ok((Arc::new(tools), clarify))
    }

    fn skills_setup(&self, id: SessionId) -> SessionSetup {
        let mut setup =
            SessionSetup::new(self.scope.clone(), Arc::clone(&self.store), id.to_string());
        setup.cwd = Some(self.options.cwd.clone());
        setup.user_home = self.launch.user_home().cloned();
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
        prompt.memory = memory.snapshot();
        let preloaded = pmt_skills::preload::preload(skills, &self.options.skills).await;
        notices.extend(preloaded.not_found_lines());
        prompt.pinned_skills = preloaded.text;
        prompt
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
        let tools = tools::session_tools(&SurfaceInputsOwned {
            scope: &self.scope,
            store: &self.store,
            session: live.id,
            registry: &self.registry,
            selection,
            capabilities: &live.capabilities,
            gate: &live.gate,
            agent: Some(&live.agent_tools),
            redactor: &self.redactor,
            context_window: None,
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
        let selection = Selection::resolve(
            &self.registry,
            &[pmt_skills::tools::TOOLSET.to_owned()],
            &[],
        )?;
        tools::session_tools(&SurfaceInputsOwned {
            scope: &self.scope,
            store: &self.store,
            session: child,
            registry: &self.registry,
            selection: &selection,
            capabilities: &live.capabilities,
            gate: &live.gate,
            agent: None,
            redactor: &self.redactor,
            context_window: None,
        })
        .await
    }
}

/// Re-reads the skills index and the memory snapshot before every prompt
/// rebuild (a `--now` change or a compaction), never in between.
fn refresher(skills: &Arc<SkillsSession>, memory: &Arc<MemorySession>) -> RefreshPrompt {
    let skills = Arc::clone(skills);
    let memory = Arc::clone(memory);
    Arc::new(move |prompt: &mut PromptSetup| {
        prompt.skills_index = skills.index_text();
        memory.refresh(prompt);
    })
}

impl Live {
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
        self.emit(Event::SessionEnd(SessionEnd {
            reason: reason.as_str().to_owned(),
        }))
        .await;
        if let Err(err) = store.end_session(self.id, reason) {
            tracing::warn!(error = %err, session = %self.id, "could not end the session");
        }
        let trajectory = self.trajectory;
        drop(self.agent);
        if let Some(writer) = trajectory.and_then(|t| Arc::try_unwrap(t).ok())
            && let Err(err) = writer.finish()
        {
            tracing::warn!(error = %err, "could not close the trajectory");
        }
    }
}

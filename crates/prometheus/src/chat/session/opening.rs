//! Parts of a session's opening that differ for a delegated child (spec:
//! subagent-delegation, Child session isolation; Child model routing;
//! Child approvals fail closed), and the agent assembly at its end.

use std::sync::{Arc, Mutex};

use pmt_agent::tools::AgentTools;
use pmt_agent::turn::{Agent, AgentSetup, EventSink, PromptSetup};
use pmt_core::capability::CapabilitySet;
use pmt_core::error::Error;
use pmt_core::id::SessionId;
use pmt_memory::session::MemorySession;
use pmt_obs::trajectory::writer::TrajectoryWriter;
use pmt_skills::session::SkillsSession;

use super::delegation::{ChildSpec, Wiring};
use super::model::SwitchableModel;
use super::tool_state::ToolState;
use super::tools::SwappableTools;
use super::{Factory, code, env, events_to, progress, summarize, tools};

/// What the agent of an opening session is assembled from.
pub(super) struct AgentInputs<'a> {
    pub id: SessionId,
    pub target: &'a pmt_providers::resolve::Target,
    pub session: &'a pmt_store::Session,
    pub child: Option<&'a ChildSpec>,
    pub clients: Option<Arc<dyn EventSink>>,
    pub model: &'a Arc<SwitchableModel>,
    pub tools: &'a Arc<SwappableTools>,
    pub tool_state: &'a Arc<ToolState>,
    pub prompt: PromptSetup,
    pub trajectory: Option<&'a Arc<TrajectoryWriter>>,
    pub environment: &'a env::Attached,
    pub agent_tools: &'a Arc<AgentTools>,
    pub delegation: &'a Wiring,
    pub summarizer: Arc<summarize::RouteSummarizer>,
    pub skills: &'a Arc<SkillsSession>,
    pub memory: &'a Arc<MemorySession>,
    pub plugin_sections: &'a Arc<Mutex<Vec<String>>>,
}

type Progress = Option<tokio::sync::mpsc::UnboundedReceiver<progress::Progress>>;

impl Factory {
    /// The session's registry, selection and surface: a child gets exactly
    /// its granted tools.
    pub(super) fn selection_for(
        &self,
        session: &pmt_store::Session,
        child: Option<&ChildSpec>,
    ) -> Result<
        (
            Arc<pmt_tools::ToolRegistry>,
            pmt_tools::Selection,
            pmt_tools::SessionSurface,
        ),
        Error,
    > {
        let (registry, selection, surface) = self.session_selection(session)?;
        Ok(match child {
            Some(spec) => (registry, spec.selection(), surface),
            None => (registry, selection, surface),
        })
    }

    /// The session's skills (their snippets run in its sandbox).
    pub(super) fn session_skills(
        &self,
        id: SessionId,
        selection: &pmt_tools::Selection,
        inline_shell: &Arc<super::skills::DeferredShell>,
        clients: Option<&super::Clients>,
    ) -> Arc<SkillsSession> {
        Arc::new(SkillsSession::new(self.skills_setup(
            id,
            selection,
            inline_shell,
            clients,
        )))
    }

    /// The session's delegation and `execute_code`.
    pub(super) fn delegation_for(
        &self,
        (session, model, registry): (
            &pmt_store::Session,
            &Arc<SwitchableModel>,
            &Arc<pmt_tools::ToolRegistry>,
        ),
        (child, clients): (Option<&ChildSpec>, Option<&super::Clients>),
        memory: &MemorySession,
        environment: &env::Attached,
    ) -> Wiring {
        let mut wiring = Wiring::new(
            self,
            session,
            model,
            registry,
            child,
            clients,
            memory.snapshot(),
        );
        wiring.attach_code(self.session_code(session.id, environment, child.is_none()));
        wiring
    }

    /// The session's prompt inputs: a child's carry no context files and
    /// its ephemeral prompt and the parent's frozen memory.
    pub(super) async fn prompt_for(
        &self,
        id: SessionId,
        (skills, memory): (&Arc<SkillsSession>, &Arc<MemorySession>),
        notices: &mut Vec<String>,
        child: Option<&ChildSpec>,
    ) -> (PromptSetup, Arc<Mutex<Vec<String>>>) {
        let (mut prompt, sections) = self.session_prompt(id, skills, memory, notices).await;
        if let Some(spec) = child {
            spec.shape_prompt(&mut prompt);
        }
        (prompt, sections)
    }

    /// The session's model port: a child that pinned its model gets no
    /// fallback chain.
    pub(super) async fn model_for(
        &self,
        target: &pmt_providers::resolve::Target,
        child: Option<&ChildSpec>,
    ) -> Result<Arc<SwitchableModel>, Error> {
        let port = match child {
            Some(spec) if spec.pinned => self.models.port_pinned(target).await?,
            _ => self.models.port(target).await?,
        };
        if let Some(spec) = child {
            port.set_body_overrides(spec.body_overrides.clone());
        }
        Ok(Arc::new(SwitchableModel::new(target.clone(), port)))
    }

    /// The session's capabilities: a child's narrowed grant, else the
    /// profile's.
    pub(super) fn capabilities_for(
        &self,
        id: SessionId,
        skills: &SkillsSession,
        child: Option<&ChildSpec>,
    ) -> Result<CapabilitySet, Error> {
        match child {
            Some(spec) => Ok(spec.capabilities(self.scope.dir(), id)),
            None => self.capabilities(id, skills),
        }
    }

    /// The session's approval gate: a child's fails closed.
    pub(super) fn gate_for(
        &self,
        id: SessionId,
        child: Option<&ChildSpec>,
        model: &Arc<SwitchableModel>,
        environment: &env::Attached,
    ) -> Result<tools::Gate, Error> {
        match child {
            Some(spec) => self.child_gate(id, spec, environment),
            None => self.gate(id, Some(model), Some(environment)),
        }
    }

    /// The session's `execute_code`, in its own sandbox.
    pub(super) fn session_code(
        &self,
        id: SessionId,
        environment: &env::Attached,
        top_level: bool,
    ) -> Arc<code::SessionCode> {
        let commands = environment.commands.as_ref();
        let place = code::Placement {
            sandbox: commands.map(|c| Arc::clone(&c.sandbox)),
            workdir: self.options.cwd.clone(),
            venv: commands
                .and_then(|c| c.env.get("VIRTUAL_ENV"))
                .map(std::path::PathBuf::from),
            top_level,
        };
        code::SessionCode::new(&self.scope, &id.to_string(), place)
    }

    /// The session's turn loop, its tool progress and its MCP registration.
    pub(super) fn agent_of(
        &self,
        inputs: AgentInputs<'_>,
    ) -> Result<(Agent, Progress, crate::mcp_host::sessions::Registration), Error> {
        let AgentInputs { id, child, .. } = inputs;
        let mut setup = AgentSetup::new(
            child.map_or_else(|| self.scope.clone(), ChildSpec::scope),
            Arc::clone(&self.store),
            id,
            Arc::clone(inputs.model) as _,
            Arc::clone(inputs.tools) as _,
            inputs.prompt,
        );
        let events = events_to(inputs.trajectory.cloned(), inputs.clients);
        let _ = inputs.environment.denials.set(Arc::downgrade(&events));
        let events = self.with_lifecycle_hooks(id, inputs.session, events);
        let (tee, progress) = progress::Tee::new(events, self.options.interactive);
        self.observe_model(inputs.model, id, tee.notices());
        let mcp = self.mcp_session(
            id,
            inputs.target,
            inputs.model,
            tee.notices(),
            inputs.agent_tools,
        );
        setup.events = Arc::new(tee);
        inputs.delegation.events_are(&setup.events);
        self.memory_and_refresh(
            &mut setup,
            inputs.summarizer,
            inputs.skills,
            inputs.memory,
            inputs.plugin_sections,
        );
        if let Some(spec) = child {
            setup.max_iterations = Some(spec.max_iterations);
            setup.memory = None;
        }
        setup.compaction_boundary = Some(Arc::clone(inputs.tool_state) as _);
        setup.context_engine = crate::context_engine::for_session(
            &self.scope,
            &self.plugins,
            id,
            &self.plugin_call(id),
        );
        self.surface_setup(&mut setup, id);
        Ok((Agent::new(setup)?, progress, mcp))
    }
}

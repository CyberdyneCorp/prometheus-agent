//! The tools and skills `prometheus mcp serve` offers (spec: plugin-system,
//! MCP server mode).
//!
//! The server is one non-interactive session of the profile without a
//! model: its tools are the toolsets of `mcp_server.expose_toolsets` (the
//! `skills` toolset only with its read-only tools), resolved and frozen
//! like any session's, and every call runs through that session's
//! dispatcher, so the profile's grants, sandbox and approval policy apply.
//! Nobody can answer an approval prompt, so a call needing one is denied.
//! The loop-owned tools (`todo`, `memory`, `clarify`...) need a turn loop
//! and are never offered.

use std::sync::Arc;

use pmt_config::schema::UnattendedApproval;
use pmt_core::cancel::CancelToken;
use pmt_core::error::Error;
use pmt_core::id::SessionId;
use pmt_mcp::serve::{ExposedTool, Host, HostFuture, SkillEntry};
use pmt_skills::session::SkillsSession;
use pmt_tools::descriptor::{Effect, Executor};
use pmt_tools::{Dispatcher, Selection, ToolCall, ToolRegistry, ToolResult};
use serde_json::{Value, json};

use super::tools::{self, SurfaceInputsOwned};
use super::{Factory, skills};
use pmt_config::SharedScope;

/// The toolset narrowed to its read-only tools when exposed.
const SKILLS: &str = "skills";

/// What an MCP client reaches.
pub struct ServeHost {
    dispatcher: Dispatcher,
    skills: Arc<SkillsSession>,
    _skills: pmt_skills::session::Attachment,
    _environment: pmt_tools::environments::Attachment,
}

impl Factory {
    /// The host of `mcp serve` for session `id` (a stored session of this
    /// factory).
    ///
    /// # Errors
    ///
    /// An unknown toolset in `mcp_server.expose_toolsets` (exit 78);
    /// registry, environment and store errors.
    pub async fn serve_host(&self, id: SessionId) -> Result<ServeHost, Error> {
        self.discover_mcp().await;
        let (registry, _) = self.session_registry()?;
        let selection = exposed(&registry, &self.scope.config().mcp_server.expose_toolsets)?;
        let inline_shell = Arc::new(skills::DeferredShell::default());
        // `mcp serve` shows no memory notices: its caller is another agent.
        let quiet = pmt_memory::notices::Notices::new(pmt_config::schema::MemoryNotifications::Off);
        let skills = Arc::new(SkillsSession::new(self.skills_setup(
            id,
            &selection,
            (&inline_shell, &quiet),
            None,
        )));
        let attached = pmt_skills::session::SkillSessions::process()
            .attach(&id.to_string(), Arc::clone(&skills));
        let capabilities = self.capabilities(id, &skills)?;
        let current = SharedScope::new(self.scope.clone());
        let target = pmt_providers::resolve::Target::default();
        // An `mcp serve` session has no trajectory: refusals go to the
        // audit log only.
        let session = self.store.session(id)?;
        let environment = self.environment(
            &session,
            &target,
            &capabilities,
            &current,
            &skills,
            &super::place::At::open(&self.options.cwd, None),
        )?;
        super::media::prime(&id.to_string()).await;
        let gate = self.gate(id, None, None)?.gate;
        // No one can answer a prompt: a call that needs approval is denied.
        let mut approvals = self.scope.config().approvals.clone();
        approvals.single_query_mode = UnattendedApproval::Deny;
        gate.set_approvals(approvals);
        skills::bind_shell(&inline_shell, environment.commands.as_ref(), &gate);
        let route = pmt_tools::variants::RouteTraits::default();
        let (dispatcher, _) = tools::dispatcher(&SurfaceInputsOwned {
            scope: &self.scope,
            store: &self.store,
            session: id,
            registry: &registry,
            selection: &selection,
            capabilities: &capabilities,
            gate: &gate,
            agent: None,
            redactor: &self.redactor,
            context_window: None,
            hooks: self
                .plugins
                .tool_hooks_with_guidance(self.plugin_call(id), &self.scope.config().plugins),
            route: &route,
        })
        .await?;
        Ok(ServeHost {
            dispatcher,
            skills,
            _skills: attached,
            _environment: environment.attachment,
        })
    }
}

/// The selection of `toolsets` without tools that need a person, with the
/// `skills` toolset narrowed to its read-only tools.
fn exposed(registry: &ToolRegistry, toolsets: &[String]) -> Result<Selection, Error> {
    let mut selection = Selection::resolve(registry, toolsets, &[])?.without_interactive(registry);
    if toolsets.iter().any(|t| t == SKILLS) {
        let writers: Vec<String> = registry
            .members(SKILLS)?
            .into_iter()
            .filter(|name| {
                registry
                    .get(name)
                    .is_ok_and(|t| t.descriptor.effect != Effect::None)
            })
            .collect();
        let named_elsewhere = |name: &String| {
            toolsets
                .iter()
                .filter(|t| *t != SKILLS)
                .any(|t| registry.members(t).is_ok_and(|m| m.contains(name)))
        };
        selection
            .tools
            .retain(|name| !writers.contains(name) || named_elsewhere(name));
    }
    Ok(selection)
}

impl Host for ServeHost {
    fn tools(&self) -> Vec<ExposedTool> {
        let surface = self.dispatcher.surface();
        surface
            .definitions()
            .iter()
            .filter_map(|definition| {
                let tool = surface.get(&definition.name)?;
                if matches!(tool.executor, Executor::AgentLoop) {
                    return None;
                }
                Some(ExposedTool {
                    name: definition.name.clone(),
                    description: definition.description.clone(),
                    parameters: definition.parameters.clone(),
                    read_only: tool.descriptor.effect == Effect::None,
                })
            })
            .collect()
    }

    fn call(&self, name: String, arguments: Value) -> HostFuture<'_, ToolResult> {
        Box::pin(async move {
            let call_id = format!("mcp_{}", ulid::Ulid::generate());
            let call = ToolCall::new(&call_id, &name, &arguments.to_string());
            self.dispatcher.dispatch(call, &CancelToken::new()).await
        })
    }

    fn skills(&self) -> Vec<SkillEntry> {
        let (catalog, config) = self.skills.catalog();
        self.skills
            .visible(&catalog, &config)
            .into_iter()
            .map(|skill| SkillEntry {
                name: skill.name.clone(),
                description: skill.description.clone(),
            })
            .collect()
    }

    fn skill(&self, name: String) -> HostFuture<'_, Result<String, String>> {
        Box::pin(async move {
            let result = pmt_skills::tools::view(&self.skills, &json!({"name": name})).await;
            if result.status.succeeded() {
                Ok(result.text_content())
            } else {
                Err(result.text_content())
            }
        })
    }
}

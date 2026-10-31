//! The session's frozen system prompt at turn start (spec: agent-turn-loop,
//! System prompt frozen per session): replay the stored bytes, or build,
//! store and announce (`prompt.rebuilt`) a new prompt when there is none,
//! the route changed or a rebuild was requested.

use pmt_core::error::{Error, codes};
use pmt_obs::trajectory::events::Event;
use pmt_obs::trajectory::payload::PromptRebuilt;

use super::model::RouteInfo;
use super::run::Turn;
use crate::prompt::frozen::{self, Frozen};
use crate::prompt::{self, PromptInputs};

impl Turn<'_> {
    pub(super) async fn freeze(&mut self, route: &RouteInfo) -> Result<Frozen, Error> {
        let setup = &self.agent.setup;
        let pending = self.agent.take_rebuild();
        let why = match frozen::stored(&setup.store, setup.session, &route.provider, &route.model)?
        {
            Ok(stored) => match pending {
                None => return Ok(stored),
                Some(why) => why,
            },
            Err(why) => why,
        };
        let tools = setup.tools.definitions();
        let inputs = self.inputs(route, tools.iter().map(|t| t.name.clone()).collect());
        let built = tokio::task::spawn_blocking(move || prompt::build(&inputs))
            .await
            .map_err(|e| Error::new(&codes::TASK_PANICKED, format!("prompt build: {e}")))??;
        let frozen = frozen::save(
            &setup.store,
            setup.session,
            &built,
            tools,
            (&route.provider, &route.model),
            &why,
        )?;
        self.notices.extend(built.warnings);
        self.emit(Event::PromptRebuilt(PromptRebuilt {
            reason: why.reason(),
            prompt_hash: Some(frozen.sha256.clone()),
        }))
        .await;
        Ok(frozen)
    }

    fn inputs(&self, route: &RouteInfo, tool_names: Vec<String>) -> PromptInputs {
        let setup = &self.agent.setup;
        let p = &setup.prompt;
        let mut config = self.config.clone();
        if let Some(name) = self.agent.personality_override() {
            config.agent.personality = name;
        }
        PromptInputs {
            config,
            profile_dir: setup.scope.dir().to_path_buf(),
            platform: p.platform.clone(),
            registered_hint: p.registered_hint.clone(),
            cwd: p.cwd.clone(),
            no_context_files: p.no_context_files,
            model: route.model.clone(),
            context_window: route.context_window,
            tool_names,
            caller_system: p.caller_system.clone(),
            skills_index: p.skills_index.clone(),
            pinned_skills: p.pinned_skills.clone(),
            memory: p.memory.clone(),
            plugin_sections: p.plugin_sections.clone(),
            session_id: setup.session,
            pass_session_id: p.pass_session_id,
            started_at: p.started_at,
        }
    }
}

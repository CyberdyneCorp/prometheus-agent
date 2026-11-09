//! The session's frozen system prompt at turn start (spec: agent-turn-loop,
//! System prompt frozen per session): replay the stored bytes, or build,
//! store and announce (`prompt.rebuilt`) a new prompt when there is none,
//! the route changed or a rebuild was requested.

use pmt_core::error::{Error, codes};
use pmt_obs::trajectory::events::Event;
use pmt_obs::trajectory::payload::PromptRebuilt;

use super::Agent;
use super::model::RouteInfo;
use super::run::Turn;
use crate::prompt::frozen::{self, Frozen, Rebuild};
use crate::prompt::{self, PromptInputs};

impl Turn<'_> {
    pub(super) async fn freeze(&mut self, route: &RouteInfo) -> Result<Frozen, Error> {
        let setup = &self.agent.setup;
        let pending = self.agent.take_rebuild();
        if matches!(pending, Some(Rebuild::Compaction))
            && let Some(boundary) = &setup.compaction_boundary
        {
            boundary.refresh_tools().await;
        }
        let (why, stored) =
            match frozen::stored(&setup.store, setup.session, &route.provider, &route.model)? {
                Ok(stored) => match pending {
                    None => return Ok(stored),
                    Some(why) => (why, Some(stored)),
                },
                Err(why) => (why, None),
            };
        let tools = setup.tools.definitions();
        let inputs = self.inputs(route, tools.iter().map(|t| t.name.clone()).collect());
        // The blocking pool does not inherit the span: its records (a
        // blocked context file) must keep the session's profile and ids.
        let span = tracing::Span::current();
        let mut built =
            tokio::task::spawn_blocking(move || span.in_scope(|| prompt::build(&inputs)))
                .await
                .map_err(|e| Error::new(&codes::TASK_PANICKED, format!("prompt build: {e}")))??;
        self.pin_and_record(&mut built);
        // Rebuilt bytes equal to the stored ones keep the stored prompt.
        let frozen = match stored {
            Some(stored)
                if stored.sha256 == prompt::hash(&built.prompt.tiers())
                    && stored.tools == tools =>
            {
                stored
            }
            _ => frozen::save(
                &setup.store,
                setup.session,
                &built,
                tools,
                (&route.provider, &route.model),
                &why,
            )?,
        };
        self.notices.extend(built.warnings);
        // A recorded rebuild changes the prefix on purpose.
        self.agent.swap_prefix(None);
        self.emit(Event::PromptRebuilt(PromptRebuilt {
            reason: why.reason(),
            prompt_hash: Some(frozen.sha256.clone()),
        }))
        .await;
        Ok(frozen)
    }

    /// A replay's recorded layers replace the fresh ones; the build is
    /// kept in the session's recording.
    fn pin_and_record(&self, built: &mut prompt::Built) {
        let setup = &self.agent.setup;
        if let Some(pins) = &setup.prompt_pins {
            built.prompt = pins.pin(setup.session, std::mem::take(&mut built.prompt));
        }
        if let Some(recording) = &setup.recording {
            recording.prompt(
                &setup.session.to_string(),
                &pmt_obs::recording::Prompt {
                    ts: jiff::Timestamp::now().to_string(),
                    sections: prompt::pins::sections(&built.prompt),
                },
            );
        }
    }

    fn inputs(&self, route: &RouteInfo, tool_names: Vec<String>) -> PromptInputs {
        self.agent.prompt_inputs(self.config, route, tool_names)
    }
}

impl Agent {
    /// What the system prompt of this session is built from on `route`.
    pub(super) fn prompt_inputs(
        &self,
        config: &pmt_config::Config,
        route: &RouteInfo,
        tool_names: Vec<String>,
    ) -> PromptInputs {
        let setup = &self.setup;
        let mut p = setup.prompt.clone();
        if let Some(refresh) = &setup.refresh_prompt {
            refresh(&mut p);
        }
        let mut config = config.clone();
        if let Some(name) = self.personality_override() {
            config.agent.personality = name;
        }
        PromptInputs {
            config,
            profile_dir: setup.scope.dir().to_path_buf(),
            platform: p.platform.clone(),
            registered_hint: p.registered_hint.clone(),
            cwd: p.cwd.clone(),
            cwd_fallback: p.cwd_fallback,
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

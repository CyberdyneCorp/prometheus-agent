//! Improver sessions on the real session factory (spec:
//! harness-evolution, Improver sessions): a hidden child of the job's
//! root, opened with a delegated child's narrowing — exactly the file
//! tools (read, write, patch, search), capabilities confined to the
//! session's private copy of the evidence, the improver's system prompt,
//! no context files, no memory and approvals that fail closed. No
//! terminal, network, browser, memory or delegation tool exists in it, so
//! a candidate's script can be read but never run.

use std::sync::Arc;
use std::time::Duration;

use pmt_agent::turn::ExitReason;
use pmt_agent::turn::events::NoEvents;
use pmt_config::ProfileScope;
use pmt_config::schema::ChildApproval;
use pmt_core::capability::{Capability, CapabilitySet};
use pmt_core::env::LaunchEnv;
use pmt_core::error::Error;
use pmt_evolve::engine::ports::{
    BoxFuture, Improver, ImproverFailure, ImproverReply, ImproverRequest,
};
use pmt_store::Store;

use super::session;
use crate::chat::session::delegation::ChildSpec;
use crate::chat::session::model::RouteFlags;
use crate::chat::session::{Factory, Options};

/// The only tools an improver has.
pub const TOOLS: &[&str] = &["read_file", "write_file", "patch", "search_files"];

/// Runs improver sessions in a profile.
pub struct HostImprover {
    /// The profile.
    pub scope: ProfileScope,
    /// The launch environment.
    pub launch: LaunchEnv,
    /// The profile's store.
    pub store: Arc<Store>,
}

/// `evolve.improver.model`, else `[auxiliary.evolution]`, else the main
/// route: as (provider, model) flags.
fn route(config: &pmt_config::Config) -> (Option<String>, Option<String>) {
    if let Some(spec) = &config.evolve.improver.model {
        return match spec.split_once('/') {
            Some((provider, model)) => (Some(provider.to_owned()), Some(model.to_owned())),
            None => (None, Some(spec.clone())),
        };
    }
    match &config.auxiliary.evolution {
        Some(task) if task.provider.is_some() || task.model.is_some() => {
            (task.provider.clone(), task.model.clone())
        }
        _ => (None, None),
    }
}

impl HostImprover {
    fn spec(request: &ImproverRequest<'_>, scope: ProfileScope) -> ChildSpec {
        let ws = std::fs::canonicalize(request.workspace)
            .unwrap_or_else(|_| request.workspace.to_path_buf());
        let capabilities: CapabilitySet =
            [Capability::read_under(&ws), Capability::write_under(&ws)]
                .into_iter()
                .collect();
        ChildSpec {
            subagent_id: format!("evolve-{}", request.label),
            depth: 1,
            pinned: true,
            fallback_chain: Some(Vec::new()),
            tools: TOOLS.iter().map(|t| (*t).to_owned()).collect(),
            capabilities,
            system: request.system.to_owned(),
            memory: None,
            max_iterations: request.max_iterations,
            scope,
            approval: ChildApproval::Deny,
            escalation_timeout: Duration::ZERO,
            parent_gate: None,
            root_events: Arc::new(NoEvents),
            oneshot: None,
            body_overrides: None,
        }
    }

    async fn run_in(
        &self,
        request: &ImproverRequest<'_>,
    ) -> Result<ImproverReply, ImproverFailure> {
        let fail = |transport: bool, message: String| ImproverFailure {
            transport,
            message,
            usd: 0.0,
        };
        let iterations = request.max_iterations;
        let scope = self.scope.patched(move |config| {
            config.memory.nudge_interval = 0;
            config.memory.memory_enabled = false;
            config.memory.user_profile_enabled = false;
            config.agent.max_iterations = iterations;
        });
        let (provider, model) = route(scope.config());
        let flags = RouteFlags {
            provider: provider.as_deref(),
            model: model.as_deref(),
            mock_script: None,
            unrecorded: false,
        };
        let options = Options {
            interactive: false,
            cwd: request.workspace.to_path_buf(),
            no_context_files: true,
            source_tag: Some(session::IMPROVER_TAG.to_owned()),
            ..Options::default()
        };
        let opened = async {
            let factory = Factory::with_store(
                scope.clone(),
                Arc::clone(&self.store),
                &self.launch,
                &flags,
                None,
                options,
            )?;
            let target = factory.models.primary(&flags).await?;
            let job_dir = self
                .scope
                .dir()
                .join("evolve")
                .join("jobs")
                .join(request.job_id);
            let root = session::job_root(&self.store, &job_dir, request.job_id)?;
            let id = session::child_row(
                &self.store,
                root,
                session::IMPROVER_TAG,
                &target,
                request.workspace,
            )?;
            let spec = Self::spec(request, scope.clone());
            let live = factory
                .open_child(id, &target, &spec, session::detached())
                .await?;
            Ok::<_, Error>((factory, live, id))
        };
        let (factory, live, id) = Box::pin(opened)
            .await
            .map_err(|e| fail(false, e.message().to_owned()))?;
        let result = Box::pin(session::turn(
            &factory,
            live,
            request.mission,
            request.cancel,
            request.timeout,
        ))
        .await;
        let usd = session::spend(&self.store, id);
        let transport = match &result {
            Ok(r) => r.exit_reason == ExitReason::ProviderFailed,
            Err(_) => false,
        };
        match session::final_text(&result) {
            Ok(text) => Ok(ImproverReply { text, usd }),
            Err(_) if matches!(&result, Ok(r) if r.exit_reason == ExitReason::EmptyResponse) => {
                Ok(ImproverReply {
                    text: String::new(),
                    usd,
                })
            }
            Err(message) => Err(ImproverFailure {
                transport,
                message,
                usd,
            }),
        }
    }
}

impl Improver for HostImprover {
    fn run<'a>(
        &'a self,
        request: ImproverRequest<'a>,
    ) -> BoxFuture<'a, Result<ImproverReply, ImproverFailure>> {
        Box::pin(async move { Box::pin(self.run_in(&request)).await })
    }

    fn model_id(&self) -> String {
        let (provider, model) = route(self.scope.config());
        let main = &self.scope.config().model;
        format!(
            "{}/{}",
            provider
                .or_else(|| main.provider.clone())
                .unwrap_or_else(|| "auto".to_owned()),
            model
                .or_else(|| main.default.clone())
                .unwrap_or_else(|| "default".to_owned())
        )
    }
}

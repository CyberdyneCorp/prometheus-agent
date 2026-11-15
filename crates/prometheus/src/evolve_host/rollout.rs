//! Rollouts on the real session factory (spec: harness-evolution,
//! Isolated rollouts; Side-effect dry-run in rollouts): a headless,
//! hidden `source = "harness"` session with the candidate harness, in a
//! fresh workspace built from the frozen task, on a sandboxed backend,
//! with every approval denied, no memory, egress limited to the model
//! providers, external tools recorded instead of run, and bounded by
//! `evolve.rollout.max_iterations` and `timeout_secs`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use pmt_config::ProfileScope;
use pmt_config::schema::{ApprovalMode, EgressMode, RolloutNetwork, UnattendedApproval};
use pmt_core::env::LaunchEnv;
use pmt_core::error::{Error, codes};
use pmt_evolve::engine::evidence::MAX_IMAGES;
use pmt_evolve::engine::ports::{
    BoxFuture, Entry, EntryKind, RecordedCall, RolloutOutcome, RolloutRequest, Rollouts,
};
use pmt_evolve::job::task::TaskMeta;
use pmt_evolve::job::workspace;
use pmt_store::Store;

use super::session;
use crate::chat::session::model::RouteFlags;
use crate::chat::session::rollout::{Recorder, RolloutRun};
use crate::chat::session::{Factory, Options};

/// The backend name of the sandboxed local backend.
pub const LOCAL_SANDBOX: &str = "local-sandbox";

/// Backends rollouts refuse: they would run unsandboxed on the host.
const REFUSED: &[&str] = &["local", "host"];

/// The terminal backend `evolve.rollout.backend` names.
///
/// # Errors
///
/// `config.invalid` for the unsandboxed host backend.
pub fn backend(name: &str) -> Result<&str, Error> {
    if REFUSED.contains(&name) {
        return Err(Error::new(
            &codes::CONFIG_INVALID,
            format!(
                "evolve.rollout.backend '{name}' is refused: rollouts never run on the unsandboxed host backend; use '{LOCAL_SANDBOX}' or a container, ssh or plugin backend"
            ),
        ));
    }
    Ok(if name == LOCAL_SANDBOX { "local" } else { name })
}

/// The hosts of the configured model providers' base URLs.
fn provider_hosts(config: &pmt_config::Config) -> Vec<String> {
    config
        .providers
        .values()
        .filter_map(|p| p.base_url.as_deref())
        .filter_map(|url| url::Url::parse(url).ok())
        .filter_map(|url| url.host_str().map(str::to_owned))
        .collect()
}

/// The profile as a rollout sees it.
fn rollout_scope(scope: &ProfileScope) -> Result<ProfileScope, Error> {
    let rollout = scope.config().evolve.rollout.clone();
    let terminal = backend(&rollout.backend)?.to_owned();
    let strict = rollout.backend == LOCAL_SANDBOX;
    let hosts = provider_hosts(scope.config());
    Ok(scope.patched(move |config| {
        config.memory.nudge_interval = 0;
        config.memory.memory_enabled = false;
        config.memory.user_profile_enabled = false;
        config.agent.max_iterations = rollout.max_iterations;
        config.approvals.mode = ApprovalMode::Manual;
        config.approvals.single_query_mode = UnattendedApproval::Deny;
        config.terminal.backend = terminal;
        if strict {
            config.security.sandbox.allow_degraded.clear();
        }
        if rollout.network == RolloutNetwork::Providers {
            config.security.egress.mode = EgressMode::Allowlist;
            config.security.egress.allow = hosts;
        }
    }))
}

/// Runs rollouts in a profile.
pub struct HostRollouts {
    /// The profile.
    pub scope: ProfileScope,
    /// The launch environment.
    pub launch: LaunchEnv,
    /// The profile's store.
    pub store: Arc<Store>,
}

impl HostRollouts {
    async fn run_in(
        &self,
        request: &RolloutRequest<'_>,
        scratch: &Path,
    ) -> Result<RolloutOutcome, Error> {
        let ws = scratch.join("workspace");
        workspace::materialize(request.task, &ws)?;
        let before = scratch.join("before");
        workspace::snapshot(&ws, &before)?;
        let meta: TaskMeta = std::fs::read_to_string(request.task.join("task.toml"))
            .ok()
            .and_then(|t| toml::from_str(&t).ok())
            .unwrap_or_default();
        let statement = std::fs::read_to_string(request.task.join("task.md"))
            .map_err(|e| Error::io(&codes::HARNESS_IO, request.task, &e))?;
        let recorder = Arc::new(Recorder::default());
        let options = Options {
            interactive: false,
            cwd: ws.clone(),
            source_tag: Some(session::ROLLOUT_TAG.to_owned()),
            rollout: Some(Arc::new(RolloutRun {
                harness: request.harness.to_path_buf(),
                toolsets: Some(meta.toolsets.clone()),
                recorder: Arc::clone(&recorder),
            })),
            ..Options::default()
        };
        let flags = RouteFlags::default();
        let factory = Factory::with_store(
            rollout_scope(&self.scope)?,
            Arc::clone(&self.store),
            &self.launch,
            &flags,
            None,
            options,
        )?;
        let target = factory.models.primary(&flags).await?;
        let job_dir = request.task.parent().unwrap_or(request.task);
        let root = session::job_root(&self.store, job_dir, request.job_id)?;
        let id = session::child_row(&self.store, root, session::ROLLOUT_TAG, &target, &ws)?;
        let live = factory
            .open_with(id, &target, Some(session::detached()))
            .await?;
        let limit = Duration::from_secs(self.scope.config().evolve.rollout.timeout_secs);
        let result = Box::pin(session::turn(
            &factory,
            live,
            &statement,
            request.cancel,
            limit,
        ))
        .await;
        let mut outcome = RolloutOutcome {
            entries: session::entries(&self.store, id),
            diff: workspace::diff(&before, &ws),
            external_calls: recorder
                .calls()
                .into_iter()
                .map(|(tool, arguments)| RecordedCall { tool, arguments })
                .collect(),
            usd: session::spend(&self.store, id),
            ..RolloutOutcome::default()
        };
        for image in workspace::new_images(&before, &ws, MAX_IMAGES) {
            let _ = std::fs::copy(
                &image,
                request.images.join(image.file_name().unwrap_or_default()),
            );
        }
        match session::final_text(&result) {
            Ok(text) if !text.trim().is_empty() => {
                outcome.ok = true;
                outcome.final_message = Some(text);
            }
            Ok(_) => outcome.error = Some("no final assistant message".to_owned()),
            Err(why) => {
                outcome
                    .entries
                    .push(Entry::new(EntryKind::Note, format!("rollout ended: {why}")));
                outcome.error = Some(why);
            }
        }
        Ok(outcome)
    }
}

impl Rollouts for HostRollouts {
    fn run<'a>(&'a self, request: RolloutRequest<'a>) -> BoxFuture<'a, RolloutOutcome> {
        Box::pin(async move {
            let scratch = scratch_dir(request.task, request.label);
            let outcome = match Box::pin(self.run_in(&request, &scratch)).await {
                Ok(outcome) => outcome,
                Err(err) => RolloutOutcome::failed(err.message()),
            };
            // The sandbox goes with the rollout, whatever happened.
            let _ = std::fs::remove_dir_all(&scratch);
            outcome
        })
    }

    fn model_id(&self) -> String {
        let model = &self.scope.config().model;
        format!(
            "{}/{}",
            model.provider.as_deref().unwrap_or("auto"),
            model.default.as_deref().unwrap_or("default")
        )
    }
}

/// `<job>/rollouts-tmp/<label>-<ulid>`.
fn scratch_dir(task: &Path, label: &str) -> PathBuf {
    task.parent()
        .unwrap_or(task)
        .join("rollouts-tmp")
        .join(format!("{label}-{}", ulid::Ulid::generate()))
}

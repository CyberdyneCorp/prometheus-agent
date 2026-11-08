//! The binary's half of scheduled jobs (spec: scheduled-jobs): what a run
//! executes ([`HostRunner`]: a fresh agent session, a sandboxed script, the
//! preflight), the profile's registries for validation ([`HostCatalog`]),
//! the driver both triggers share ([`drive`]: the daemon's ticker and
//! `prometheus cron tick`), and the `cronjob` model tool ([`tool`]).

mod agent;
pub mod blueprint_rpc;
pub mod blueprints;
pub mod caller;
mod fetch;
mod inbox;
pub mod notepad;
pub mod retired;
mod script;
pub mod tool;

pub use inbox::inbox;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use jiff::tz::TimeZone;
use pmt_agent::turn::model::BoxFuture;
use pmt_config::ProfileScope;
use pmt_core::env::LaunchEnv;
use pmt_core::error::Error;
use pmt_core::secret::Redactor;
use pmt_cron::clock::SystemClock;
use pmt_cron::exec::{AgentOutcome, AgentRequest, Exec, Runner, ScriptOutcome};
use pmt_cron::job::Job;
use pmt_cron::ops::Profile;
use pmt_cron::target::{Resolved, Target, resolve};
use pmt_cron::validate::Catalog;
use pmt_skills::bundled::Bundled;
use pmt_store::Store;
use pmt_store::cron::RunRow;

/// `<profile>/scripts`.
#[must_use]
pub fn scripts_dir(scope: &ProfileScope) -> PathBuf {
    scope.path("scripts")
}

/// The working directory of scheduled runs: `<profile>/cron/workspace`.
#[must_use]
pub fn workspace(scope: &ProfileScope) -> PathBuf {
    scope.path("cron").join("workspace")
}

/// The profile zone (`cron.timezone`, else the host's).
///
/// # Errors
///
/// `config.timezone` for an unknown zone.
pub fn profile_zone(scope: &ProfileScope) -> Result<TimeZone, Error> {
    pmt_obs::time::profile_zone(&scope.config().cron)
}

/// The profile's skills, toolsets and providers, as job validation asks.
pub struct HostCatalog {
    scope: ProfileScope,
    user_home: Option<PathBuf>,
}

impl HostCatalog {
    /// The catalog of `scope`.
    #[must_use]
    pub fn new(scope: &ProfileScope, launch: &LaunchEnv) -> Self {
        Self {
            scope: scope.clone(),
            user_home: launch.user_home().cloned(),
        }
    }

    fn skills(&self) -> pmt_skills::Catalog {
        pmt_skills::eject::profile_catalog_at(
            &self.scope.config().skills,
            self.scope.dir(),
            None,
            self.user_home.as_deref(),
            Bundled::builtin(),
        )
    }

    /// The bodies of `names` (unknown names are skipped).
    #[must_use]
    pub fn skill_bodies(&self, names: &[String]) -> String {
        let catalog = self.skills();
        names
            .iter()
            .filter_map(|name| catalog.lookup(name).ok())
            .map(|skill| skill.body.clone())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl Catalog for HostCatalog {
    fn skill_exists(&self, name: &str) -> bool {
        self.skills().lookup(name).is_ok()
    }

    fn toolset_exists(&self, name: &str) -> bool {
        crate::tools_cmd::expand_toolsets(&[name.to_owned()]).is_ok()
    }

    fn check_model(&self, provider: Option<&str>, model: Option<&str>) -> Result<(), String> {
        let registry =
            pmt_providers::registry::ProviderRegistry::for_profile(self.scope.config(), Vec::new())
                .map_err(|e| e.message().to_owned())?;
        let named = provider.or_else(|| model.and_then(|m| m.split_once('/').map(|(p, _)| p)));
        match named {
            Some(name) if registry.canonical(name).is_none() => {
                Err(format!("unknown provider '{name}'"))
            }
            _ => Ok(()),
        }
    }

    fn profile_exists(&self, name: &str) -> bool {
        pmt_config::ProfileName::parse(name).is_ok_and(|name| {
            pmt_config::Home::open(self.scope.home())
                .ok()
                .and_then(|home| home.existing_profile(&name).ok())
                .is_some()
        })
    }

    fn secret_reserved(&self, name: &str) -> bool {
        reserved_secret(&self.scope, name)
    }
}

/// Whether secret `name` is a provider credential (an API key of a
/// registered provider, any label, or a subscription token) or owned by a
/// surface (the canonical surface-only list: built-in platform tokens
/// whether or not the config names them, plus every secret the gateway
/// configuration names).
fn reserved_secret(scope: &ProfileScope, name: &str) -> bool {
    if name.starts_with("OAUTH_") {
        return true;
    }
    let config = scope.config();
    if pmt_config::secrets::surface::SurfaceOnly::of(config).contains(name) {
        return true;
    }
    pmt_providers::registry::ProviderRegistry::for_profile(config, Vec::new()).is_ok_and(
        |registry| {
            registry
                .iter()
                .flat_map(pmt_providers::Provider::secret_names)
                .any(|key| {
                    name == key
                        || name
                            .strip_prefix(key.as_str())
                            .is_some_and(|r| r.starts_with('_'))
                })
        },
    )
}

/// Runs jobs of one profile: agent sessions through the session factory,
/// scripts under the profile's sandbox.
pub struct HostRunner {
    scope: ProfileScope,
    launch: LaunchEnv,
    store: Arc<Store>,
    catalog: HostCatalog,
    redactor: Redactor,
}

impl HostRunner {
    /// The runner of `scope`'s jobs over its open `store`.
    #[must_use]
    pub fn new(scope: &ProfileScope, launch: &LaunchEnv, store: Arc<Store>) -> Self {
        Self {
            scope: scope.clone(),
            launch: launch.clone(),
            store,
            catalog: HostCatalog::new(scope, launch),
            redactor: scope.secrets().redactor().unwrap_or_default(),
        }
    }

    /// The operations' view of the profile.
    #[must_use]
    pub fn profile<'a>(&'a self, scripts: &'a Path) -> Profile<'a> {
        Profile {
            store: &self.store,
            scripts_dir: scripts,
            zone: profile_zone(&self.scope).unwrap_or(TimeZone::UTC),
            catalog: &self.catalog,
        }
    }

    /// The profile's `[cron]` settings.
    #[must_use]
    pub fn config(&self) -> pmt_config::schema::CronConfig {
        self.scope.config().cron.clone()
    }

    /// The profile.
    #[must_use]
    pub fn scope(&self) -> &ProfileScope {
        &self.scope
    }

    /// The profile's store.
    #[must_use]
    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    /// Executes `run` of `job` to its end.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub async fn execute(&self, job: &Job, run: &RunRow) -> Result<RunRow, Error> {
        let scripts = scripts_dir(&self.scope);
        let config = self.scope.config().cron.clone();
        let zone = profile_zone(&self.scope).unwrap_or(TimeZone::UTC);
        let ctx = Exec {
            store: &self.store,
            profile: self.scope.name().as_str(),
            zone: &zone,
            scripts_dir: &scripts,
            config: &config,
            runner: self,
            clock: &SystemClock,
        };
        pmt_cron::exec::execute(&ctx, job, run).await
    }
}

impl Runner for HostRunner {
    fn preflight<'a>(&'a self, job: &'a Job) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            if let Some(skill) = job
                .spec
                .skills
                .iter()
                .find(|s| !self.catalog.skill_exists(s))
            {
                return Err(format!("skill '{skill}' is not installed"));
            }
            if let Some(missing) = self.missing_adapter(job) {
                return Err(missing);
            }
            if job.spec.no_agent {
                return Ok(());
            }
            agent::credential(&self.scope, &self.launch, job).await
        })
    }

    fn skill_text(&self, job: &Job) -> String {
        self.catalog.skill_bodies(&job.spec.skills)
    }

    fn run_script<'a>(
        &'a self,
        job: &'a Job,
        script: &'a Path,
        timeout: std::time::Duration,
    ) -> BoxFuture<'a, ScriptOutcome> {
        Box::pin(script::run(&self.scope, job, script, timeout))
    }

    fn fetch<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(fetch::get(&self.scope, url))
    }

    fn run_agent(&self, request: AgentRequest) -> BoxFuture<'_, AgentOutcome> {
        Box::pin(agent::run(
            &self.scope,
            &self.launch,
            Arc::clone(&self.store),
            request,
        ))
    }

    fn redact(&self, text: &str) -> String {
        self.redactor.redact_all(text)
    }
}

impl HostRunner {
    /// The first delivery platform the job's profile does not enable.
    fn missing_adapter(&self, job: &Job) -> Option<String> {
        job.spec
            .deliver
            .iter()
            .filter_map(|t| Target::parse(t).ok())
            .filter_map(|t| match resolve(&t, job.origin.as_ref()) {
                Resolved::Messaging(to) => Some(to),
                _ => None,
            })
            .map(|to| to.split(':').next().unwrap_or_default().to_owned())
            .find(|platform| !self.adapter_configured(platform))
            .map(|platform| not_configured(&platform, &self.scope))
    }

    fn adapter_configured(&self, platform: &str) -> bool {
        self.scope
            .config()
            .gateway
            .platforms
            .get(platform)
            .is_some_and(|p| p.enabled)
    }
}

/// `<platform> not configured for profile <profile>`.
#[must_use]
pub fn not_configured(platform: &str, scope: &ProfileScope) -> String {
    format!(
        "{platform} not configured for profile {}",
        scope.name().as_str()
    )
}

/// The runs one tick started, each finishing on its own task.
pub type Running = tokio::task::JoinSet<Result<RunRow, Error>>;

/// Claims what is due in the runner's profile at `now` and starts it; a
/// run waits for one of `permits` (`cron.max_parallel_jobs`, shared
/// across ticks) before it executes.
///
/// # Errors
///
/// Store errors of the claim, `config.timezone`.
pub fn dispatch(
    runner: &Arc<HostRunner>,
    now: jiff::Timestamp,
    permits: &Arc<tokio::sync::Semaphore>,
) -> Result<Running, Error> {
    let scope = &runner.scope;
    let config = scope.config().cron.clone();
    let zone = profile_zone(scope)?;
    let claimed = pmt_cron::scheduler::tick(&runner.store, scope.home(), now, &zone, &config)?;
    let mut tasks = Running::new();
    for (job, run) in claimed {
        let runner = Arc::clone(runner);
        let permits = Arc::clone(permits);
        tasks.spawn(async move {
            let _permit = permits.acquire_owned().await;
            runner.execute(&job, &run).await
        });
    }
    Ok(tasks)
}

/// The permits of a profile's concurrent runs.
#[must_use]
pub fn permits(config: &pmt_config::schema::CronConfig) -> Arc<tokio::sync::Semaphore> {
    Arc::new(tokio::sync::Semaphore::new(
        usize::try_from(config.max_parallel_jobs.max(1)).unwrap_or(1),
    ))
}

/// Claims and executes what is due at `now`, waiting for every run.
///
/// # Errors
///
/// As [`dispatch`].
pub async fn drive(runner: Arc<HostRunner>, now: jiff::Timestamp) -> Result<Vec<RunRow>, Error> {
    let permits = permits(&runner.config());
    let mut tasks = dispatch(&runner, now, &permits)?;
    let mut runs = Vec::new();
    while let Some(done) = tasks.join_next().await {
        match done {
            Ok(Ok(run)) => runs.push(run),
            Ok(Err(err)) => tracing::warn!(error = %err, "cron run failed to record"),
            Err(err) => tracing::warn!(error = %err, "cron run panicked"),
        }
    }
    Ok(runs)
}

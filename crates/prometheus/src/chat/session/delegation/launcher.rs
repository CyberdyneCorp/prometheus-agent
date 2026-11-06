//! Opens delegated children as real sessions (spec: subagent-delegation,
//! Child session isolation; Profile routes run the child inside the target
//! `ProfileScope`; Child processes are reaped or handed off).
//!
//! A child is a new `subagent` session (`parent_session_id` and
//! `root_session_id` set, recorded in the launching profile's store) built
//! by a factory of its profile: the launching one, or the profile a router
//! chose, whose configuration, secrets and sandbox policy it runs under
//! (never the launching profile's secrets). Its grant is intersected with
//! both profiles' grants.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use pmt_agent::turn::model::BoxFuture;
use pmt_agent::turn::{EventSink, ExitReason, Interrupt, TurnInput, TurnOptions};
use pmt_config::grants::{GrantsFile, SessionGrants, Subject, baseline};
use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
use pmt_core::cancel::CancelToken;
use pmt_core::capability::CapabilitySet;
use pmt_core::env::LaunchEnv;
use pmt_core::error::{Error, codes};
use pmt_core::id::SessionId;
use pmt_core::secret::SecretString;
use pmt_delegation::budget::{ChildBudget, OneShotBudget};
use pmt_delegation::host::{ChildPlan, ChildSession, Closed, Launcher, TurnEnd, TurnOutcome};
use pmt_skills::secrets::SecretPrompt;
use pmt_store::{EndReason, LineageKind, NewSession, Origin, Source, Store};
use pmt_tools::approval::CommandGate;
use pmt_tools::environments::Environments;

use super::super::model::SwitchableModel;
use super::super::{Clients, Factory, Live, Options};
use super::{ChildSpec, OwnedFlags};

/// How long a child's background processes get to stop at its end.
const REAP_WITHIN: Duration = Duration::from_secs(10);

/// Factories that open children, per (profile, launching store, working
/// directory, route flags): built once, shared by every child of the
/// process.
type FactoryKey = (PathBuf, PathBuf, PathBuf, Option<PathBuf>);
static FACTORIES: LazyLock<Mutex<HashMap<FactoryKey, Arc<Factory>>>> =
    LazyLock::new(Mutex::default);

/// What a session's children are opened from.
pub struct ChildLauncher {
    scope: ProfileScope,
    store: Arc<Store>,
    launch: LaunchEnv,
    flags: OwnedFlags,
    cwd: PathBuf,
    daemon: Option<pmt_tools::approval::selfterm::Daemon>,
    model: Arc<SwitchableModel>,
    gate: Arc<OnceLock<Arc<CommandGate>>>,
    root_events: Arc<dyn EventSink>,
    oneshot: Option<Arc<OneShotBudget>>,
}

impl ChildLauncher {
    /// The launcher of the session `factory` opens on `model`.
    pub fn new(
        factory: &Factory,
        model: Arc<SwitchableModel>,
        gate: Arc<OnceLock<Arc<CommandGate>>>,
        root_events: Arc<dyn EventSink>,
        oneshot: Option<Arc<OneShotBudget>>,
    ) -> Self {
        Self {
            scope: factory.scope.clone(),
            store: Arc::clone(&factory.store),
            launch: factory.launch.clone(),
            flags: factory.route_flags.clone(),
            cwd: factory.options.cwd.clone(),
            daemon: factory.daemon.clone(),
            model,
            gate,
            root_events,
            oneshot,
        }
    }

    /// The child's profile and, on a profile route, that profile's own
    /// capabilities (the grant is intersected with them).
    fn profile(&self, name: Option<&str>) -> Result<(ProfileScope, Option<CapabilitySet>), Error> {
        let Some(name) = name.filter(|n| *n != self.scope.name().as_str()) else {
            return Ok((self.scope.clone(), None));
        };
        let missing = || {
            Error::new(
                &codes::PROFILE_NOT_FOUND,
                format!("Profile '{name}' does not exist (see 'prometheus profile list')."),
            )
        };
        let profile_name = ProfileName::parse(name).map_err(|_| missing())?;
        let dir = Home::open(self.scope.home())?
            .existing_profile(&profile_name)
            .map_err(|_| missing())?;
        // Another profile never sees the launch environment's secrets.
        let scope = ProfileScope::open(dir, &Overrides::default(), Launch::Other)?;
        let grants = SessionGrants::new(
            baseline(&scope.config().security),
            GrantsFile::of(scope.profile()),
        );
        let capabilities = grants.capabilities(&Subject::Profile)?;
        Ok((scope, Some(capabilities)))
    }

    /// The factory that opens children of `scope`.
    fn factory(&self, scope: &ProfileScope) -> Result<Arc<Factory>, Error> {
        let key = (
            scope.dir().to_path_buf(),
            self.scope.dir().to_path_buf(),
            self.cwd.clone(),
            self.flags.borrow().mock_script.map(PathBuf::from),
        );
        if let Some(factory) = FACTORIES
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&key)
        {
            return Ok(Arc::clone(factory));
        }
        let options = Options {
            interactive: false,
            cwd: self.cwd.clone(),
            ..Options::default()
        };
        let factory = Factory::with_store(
            scope.clone(),
            Arc::clone(&self.store),
            &self.launch,
            &self.flags.borrow(),
            None,
            options,
        )?;
        let factory = match &self.daemon {
            Some(daemon) => factory.hosted_by(daemon.clone()),
            None => factory,
        };
        let factory = Arc::new(factory);
        FACTORIES
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key, Arc::clone(&factory));
        Ok(factory)
    }

    async fn open_child(&self, plan: &ChildPlan) -> Result<Arc<dyn ChildSession>, Error> {
        let (scope, profile_caps) = self.profile(plan.profile.as_deref())?;
        let factory = self.factory(&scope)?;
        let pinned = plan.model.model.is_some();
        let target = match &plan.model.model {
            Some(model) => factory.models.pinned_target(model).await?,
            None => self.model.target(),
        };
        let capabilities = match &profile_caps {
            Some(profile) => plan.grant.capabilities.intersect(profile),
            None => plan.grant.capabilities.clone(),
        };
        let spec = ChildSpec {
            subagent_id: plan.subagent_id.clone(),
            depth: plan.depth,
            pinned,
            tools: plan.grant.tools.clone(),
            capabilities,
            system: plan.system.clone(),
            memory: plan.memory.clone(),
            max_iterations: plan.budget.max_iterations,
            scope: budget_scope(&scope, &plan.budget),
            approval: plan.approvals.policy,
            escalation_timeout: plan.approvals.timeout,
            parent_gate: self.gate.get().cloned(),
            root_events: Arc::clone(&self.root_events),
            oneshot: self.oneshot.clone(),
        };
        let id = self.child_session(plan.parent, &target)?;
        let clients = Clients {
            events: Arc::clone(&plan.events),
            secrets: Arc::new(NoPrompt),
            completions: None,
            sudo: None,
            delegation: None,
        };
        let live = factory.open_child(id, &target, &spec, clients).await?;
        let model = format!(
            "{}/{}",
            target.provider,
            target.model.as_deref().unwrap_or_default()
        );
        Ok(Arc::new(LiveChild {
            live: Arc::new(live),
            store: Arc::clone(&self.store),
            model,
        }))
    }

    /// The child's session row, in the launching profile's store.
    fn child_session(
        &self,
        parent: SessionId,
        target: &pmt_providers::resolve::Target,
    ) -> Result<SessionId, Error> {
        let mut new = NewSession::child(Source::Subagent, parent, LineageKind::Delegate);
        new.origin = Origin::Subagent { parent };
        new.provider = Some(target.provider.clone());
        new.model.clone_from(&target.model);
        new.cwd = Some(self.cwd.to_string_lossy().into_owned());
        Ok(self.store.create_session(&new)?.id)
    }
}

/// `scope` with the child's budget share as its session budget, so the
/// turn loop ends the child with a budget exit when the share is spent.
fn budget_scope(scope: &ProfileScope, budget: &ChildBudget) -> ProfileScope {
    if budget.max_cost_usd.is_none() && budget.max_tokens.is_none() {
        return scope.clone();
    }
    let budget = *budget;
    scope.patched(move |config| {
        if let Some(usd) = budget.max_cost_usd {
            config.budgets.session_usd = usd;
        }
        if let Some(tokens) = budget.max_tokens {
            config.budgets.session_tokens = tokens;
        }
    })
}

impl Launcher for ChildLauncher {
    fn open<'a>(
        &'a self,
        plan: &'a ChildPlan,
    ) -> BoxFuture<'a, Result<Arc<dyn ChildSession>, Error>> {
        Box::pin(self.open_child(plan))
    }
}

/// Children cannot be asked for secrets.
struct NoPrompt;

impl SecretPrompt for NoPrompt {
    fn ask(&self, _: &str, _: &str, _: Option<&str>) -> Option<SecretString> {
        None
    }
}

/// A running child session.
struct LiveChild {
    live: Arc<Live>,
    store: Arc<Store>,
    model: String,
}

impl ChildSession for LiveChild {
    fn id(&self) -> SessionId {
        self.live.id
    }

    fn model(&self) -> String {
        self.model.clone()
    }

    fn cwd(&self) -> Option<PathBuf> {
        Some(self.live.cwd.clone())
    }

    fn turn(&self, input: String, interrupt: Interrupt) -> BoxFuture<'_, TurnOutcome> {
        Box::pin(async move {
            let options = TurnOptions {
                interrupt,
                ..TurnOptions::new(&CancelToken::new())
            };
            let live = &self.live;
            match live
                .agent
                .run_turn(Some(&live.scope), live.id, TurnInput::text(input), options)
                .await
            {
                Ok(done) => TurnOutcome {
                    end: end_of(done.exit_reason),
                    text: done.final_response,
                    iterations: done.iterations,
                    usage: done.usage,
                    error: done
                        .error
                        .map(|e| (e.code().code.to_owned(), e.message().to_owned())),
                },
                Err(err) => TurnOutcome {
                    text: None,
                    end: TurnEnd::Error,
                    iterations: 0,
                    usage: pmt_agent::turn::TurnUsage::default(),
                    error: Some((err.code().code.to_owned(), err.message().to_owned())),
                },
            }
        })
    }

    fn steer(&self, text: &str) -> bool {
        self.live.agent.steer(text).is_ok()
    }

    fn undelivered_steers(&self) -> Vec<String> {
        self.live.agent.take_undelivered_steers()
    }

    fn close(&self) -> BoxFuture<'_, Closed> {
        Box::pin(async move {
            let orphaned = reap(&self.live.id.to_string()).await;
            self.live.end(&self.store, EndReason::JobComplete).await;
            Closed {
                orphaned_processes: orphaned,
            }
        })
    }
}

/// Stops the child's background processes (handed-off ones belong to the
/// parent by now); those still running after [`REAP_WITHIN`] are
/// orphaned.
async fn reap(session: &str) -> Vec<String> {
    let Some(terminal) = Environments::process()
        .session(session)
        .and_then(|env| env.terminal.clone())
    else {
        return Vec::new();
    };
    let processes = terminal.processes();
    if tokio::time::timeout(REAP_WITHIN, processes.close())
        .await
        .is_err()
    {
        tracing::warn!(session, "subagent processes did not stop in time");
    }
    processes.running()
}

fn end_of(reason: ExitReason) -> TurnEnd {
    match reason {
        ExitReason::Completed => TurnEnd::FinalAnswer,
        ExitReason::IterationLimit => TurnEnd::MaxIterations,
        ExitReason::TokenBudgetExhausted
        | ExitReason::CostBudgetExhausted
        | ExitReason::RunBudgetExhausted => TurnEnd::BudgetExhausted,
        ExitReason::InterruptedByUser | ExitReason::InterruptedBySystem => TurnEnd::Interrupted,
        _ => TurnEnd::Error,
    }
}

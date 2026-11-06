//! The session's model (spec: model-providers; cli, Stable exit codes):
//! the route a new session resolves from the chat flags and the profile,
//! the mock provider when asked for, and a port `/model` can point at
//! another route between turns.

use std::path::Path;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use pmt_agent::turn::model::{
    BoxFuture, CredentialWatch, DeltaSink, ModelFailure, ModelPort, ModelReply, ProviderModel,
    ProviderSetup, RouteInfo,
};
use pmt_agent::vision::AuxDescriber;
use pmt_config::ProfileScope;
use pmt_config::schema::ReasoningLevel;
use pmt_core::error::{Error, codes};
use pmt_obs::usage::{Prices, UsageRecorder};
use pmt_providers::ModelCaller;
use pmt_providers::ProviderRegistry;
use pmt_providers::auth::Accounts;
use pmt_providers::auxiliary::{self, AuxArgs, AuxCaller, AuxContext, AuxTask};
use pmt_providers::cost::CallContext;
use pmt_providers::fallback::MeteredConfirm;
use pmt_providers::health::StoreHealth;
use pmt_providers::metadata::Catalog;
use pmt_providers::mock::Mock;
use pmt_providers::request::Request;
use pmt_providers::resolve::{
    AdHoc, Layer, Resolver, Route, Selection, Target, TcpProbe, Trace, requested,
};
use pmt_providers::switch::SessionModel;
use pmt_store::Store;

/// The exact message of a profile with nothing to talk to.
pub const NO_MODEL: &str = "no model configured; run: prometheus setup model";

/// What builds the session's routes: the profile's providers, catalog,
/// credentials and the mock provider.
pub struct Models {
    setup: Arc<ProviderSetupParts>,
    caller: ModelCaller,
    /// Auxiliary calls (image descriptions) on the same client, mock,
    /// usage recorder and credentials as the main route.
    aux: Arc<AuxCaller>,
    /// `privacy.redact_pii`: what every request is redacted with.
    pii: Option<Arc<pmt_core::pii::Pii>>,
}

/// The shareable parts of a [`ProviderSetup`].
pub(super) struct ProviderSetupParts {
    pub(super) config: Arc<pmt_config::Config>,
    registry: Arc<ProviderRegistry>,
    secrets: pmt_config::SecretsHandle,
    accounts: Arc<Accounts>,
    pub(super) catalog: Arc<Catalog>,
    ad_hoc: Option<AdHoc>,
    health: Arc<StoreHealth>,
    metered_confirm: Option<Arc<dyn MeteredConfirm>>,
}

impl ProviderSetupParts {
    fn setup(&self) -> ProviderSetup {
        ProviderSetup {
            config: Arc::clone(&self.config),
            registry: Arc::clone(&self.registry),
            secrets: Arc::new(self.secrets.clone()),
            extras: Arc::clone(&self.accounts) as _,
            probe: Arc::new(TcpProbe {
                timeout: Duration::from_millis(500),
            }),
            catalog: Arc::clone(&self.catalog),
            health: Arc::clone(&self.health) as _,
            ad_hoc: self.ad_hoc.clone(),
            metered_confirm: self.metered_confirm.clone(),
        }
    }

    pub(super) fn resolver(&self) -> Resolver<'_> {
        static PROBE: TcpProbe = TcpProbe {
            timeout: Duration::from_millis(500),
        };
        Resolver {
            registry: &self.registry,
            config: &self.config,
            secrets: &self.secrets,
            extras: &*self.accounts,
            probe: &PROBE,
            ad_hoc: self.ad_hoc.clone(),
        }
    }
}

/// Builds model ports of a profile's routes.
#[derive(Clone)]
pub(crate) struct PortMaker {
    setup: Arc<ProviderSetupParts>,
    caller: ModelCaller,
}

impl PortMaker {
    /// The port serving `target`.
    ///
    /// # Errors
    ///
    /// Resolution errors of the target or the chain.
    pub(crate) async fn port(&self, target: &Target) -> Result<Arc<ProviderModel>, Error> {
        let model = ProviderModel::start(self.setup.setup(), self.caller.clone(), target).await?;
        Ok(Arc::new(model))
    }

    /// The port serving `target` alone, without the fallback chain.
    ///
    /// # Errors
    ///
    /// Resolution errors of the target.
    pub(crate) async fn port_pinned(&self, target: &Target) -> Result<Arc<ProviderModel>, Error> {
        let model =
            ProviderModel::start_pinned(self.setup.setup(), self.caller.clone(), target).await?;
        Ok(Arc::new(model))
    }
}

/// The chat flags that pick the route.
#[derive(Debug, Clone, Default)]
pub struct RouteFlags<'a> {
    /// `--provider`.
    pub provider: Option<&'a str>,
    /// `-m/--model`.
    pub model: Option<&'a str>,
    /// `--mock-script`.
    pub mock_script: Option<&'a Path>,
}

impl Models {
    /// The profile's model side for a session.
    ///
    /// # Errors
    ///
    /// Registry, store and mock-script errors.
    pub fn open(
        scope: &ProfileScope,
        store: &Arc<Store>,
        user_home: Option<&Path>,
        flags: &RouteFlags<'_>,
        ad_hoc: Option<AdHoc>,
    ) -> Result<Self, Error> {
        let config = scope.config();
        let registry = ProviderRegistry::for_profile(config, Vec::new())?;
        let catalog = Arc::new(Catalog::new(config, Some(scope.path("cache/models"))));
        let accounts = Arc::new(crate::accounts::open(scope, config, &registry, user_home)?);
        let mock = Arc::new(Mock::for_session(flags.mock_script, config, scope.dir())?);
        let prices: Arc<dyn Prices> = catalog.clone();
        let client = crate::model_cmd::client(config)?;
        let recorder = Arc::new(UsageRecorder::new(Arc::clone(store), prices));
        let health =
            Arc::new(StoreHealth::new(Arc::clone(store), config).with_pool(Arc::clone(store) as _));
        let aux = AuxCaller::new(client.clone())
            .with_mock(Arc::clone(&mock))
            .with_recorder(Arc::clone(&recorder))
            .with_credentials(Arc::clone(&accounts) as _);
        let caller = ModelCaller::new(client)
            .with_mock(mock)
            .with_recorder(recorder)
            .with_credentials(Arc::clone(&accounts) as _)
            .with_observer(Arc::clone(&health) as _);
        // `privacy.redact_pii`: every model request of the profile, main and
        // auxiliary, leaves the process redacted.
        let pii = pmt_config::privacy::pii(scope.dir(), &config.privacy)?;
        let (aux, caller) = match &pii {
            Some(pii) => (
                aux.with_pii(Arc::clone(pii)),
                caller.with_pii(Arc::clone(pii)),
            ),
            None => (aux, caller),
        };
        Ok(Self {
            setup: Arc::new(ProviderSetupParts {
                config: Arc::new(config.clone()),
                registry: Arc::new(registry),
                secrets: scope.secrets().clone(),
                accounts,
                catalog,
                ad_hoc,
                health,
                metered_confirm: None,
            }),
            caller,
            aux: Arc::new(aux),
            pii,
        })
    }

    /// Asks `confirm` before a fallback moves a session from a
    /// subscription to a metered credential when the provider sets
    /// `confirm_metered_fallback` (an interactive session); without it
    /// such a move fails the turn.
    #[must_use]
    pub fn with_metered_confirm(mut self, confirm: Arc<dyn MeteredConfirm>) -> Self {
        let parts = &self.setup;
        self.setup = Arc::new(ProviderSetupParts {
            config: Arc::clone(&parts.config),
            registry: Arc::clone(&parts.registry),
            secrets: parts.secrets.clone(),
            accounts: Arc::clone(&parts.accounts),
            catalog: Arc::clone(&parts.catalog),
            ad_hoc: parts.ad_hoc.clone(),
            health: Arc::clone(&parts.health),
            metered_confirm: Some(confirm),
        });
        self
    }

    /// Hashes messaging user `id` in every later request (a gateway
    /// session's platform user), when PII is redacted.
    pub fn register_user_id(&self, id: &str) {
        if let Some(pii) = &self.pii {
            pii.register_id(id);
        }
    }

    /// Adds the model providers the profile's plugins registered (spec:
    /// plugin-system, Model-provider plugins; they join at the session
    /// boundary) with their request hooks, and the plugins'
    /// `transform_api_error_classification` hooks.
    ///
    /// # Errors
    ///
    /// The registry refuses a plugin descriptor (a name clash).
    pub fn with_plugins(
        mut self,
        scope: &ProfileScope,
        plugins: &crate::plugins_host::ProfilePlugins,
        cwd: &Path,
    ) -> Result<Self, Error> {
        let loaded = plugins.current();
        let models = loaded.surfaces.models.clone();
        if !models.is_empty() {
            let registered = crate::plugins_host::providers::registered(&models);
            let registry = ProviderRegistry::for_profile(scope.config(), registered)?;
            let parts = &self.setup;
            self.setup = Arc::new(ProviderSetupParts {
                config: Arc::clone(&parts.config),
                registry: Arc::new(registry),
                secrets: parts.secrets.clone(),
                accounts: Arc::clone(&parts.accounts),
                catalog: Arc::clone(&parts.catalog),
                ad_hoc: parts.ad_hoc.clone(),
                health: Arc::clone(&parts.health),
                metered_confirm: parts.metered_confirm.clone(),
            });
            self.caller = self.caller.with_request_hooks(Arc::new(
                pmt_plugins::surfaces::ModelRequestHooks::new(models),
            ));
        }
        let call = pmt_plugins::host::CallInfo {
            session_id: None,
            surface: "cli".to_owned(),
            cwd: Some(cwd.display().to_string()),
        };
        self.caller = self.caller.with_classifier(plugins.error_classifier(call));
        Ok(self)
    }

    /// The model side of a session after `/provider reload --now` (spec:
    /// model-providers, Provider registry changes apply at session
    /// boundaries): the registry rebuilt from the profile's current config
    /// and plugins, with the ids whose descriptor or settings changed.
    ///
    /// # Errors
    ///
    /// The registry refuses a provider block or plugin descriptor.
    pub fn reloaded(
        &self,
        scope: &ProfileScope,
        plugins: &crate::plugins_host::ProfilePlugins,
    ) -> Result<(Self, Vec<String>), Error> {
        let models = plugins.current().surfaces.models.clone();
        let registered = crate::plugins_host::providers::registered(&models);
        let registry = ProviderRegistry::for_profile(scope.config(), registered)?;
        let old = &self.setup.registry;
        let mut changed: Vec<String> = registry
            .iter()
            .filter(|p| old.lookup(p.id()).ok() != Some(*p))
            .map(|p| p.id().to_owned())
            .collect();
        changed.extend(
            old.iter()
                .filter(|p| registry.lookup(p.id()).is_err())
                .map(|p| p.id().to_owned()),
        );
        let parts = &self.setup;
        let caller = if models.is_empty() {
            self.caller.clone()
        } else {
            self.caller.clone().with_request_hooks(Arc::new(
                pmt_plugins::surfaces::ModelRequestHooks::new(models),
            ))
        };
        let reloaded = Self {
            setup: Arc::new(ProviderSetupParts {
                config: Arc::new(scope.config().clone()),
                registry: Arc::new(registry),
                secrets: parts.secrets.clone(),
                accounts: Arc::clone(&parts.accounts),
                catalog: Arc::clone(&parts.catalog),
                ad_hoc: parts.ad_hoc.clone(),
                health: Arc::clone(&parts.health),
                metered_confirm: parts.metered_confirm.clone(),
            }),
            caller,
            aux: Arc::clone(&self.aux),
            pii: self.pii.clone(),
        };
        Ok((reloaded, changed))
    }

    /// The session's first route: flags over the profile config. A profile
    /// with no model, no provider and no launch endpoint exits 78 with
    /// [`NO_MODEL`].
    ///
    /// # Errors
    ///
    /// `model.not_configured`, resolution errors.
    pub async fn primary(&self, flags: &RouteFlags<'_>) -> Result<Target, Error> {
        let parts = &self.setup;
        if let Some(target) = self.flag_target(flags) {
            let route = parts
                .resolver()
                .resolve_target(&target, &mut Trace::default())
                .await?;
            return Ok(Target {
                provider: route.provider,
                model: Some(route.model),
                ..target
            });
        }
        let selection = Selection {
            flag: Layer {
                provider: flags.provider.map(str::to_owned),
                model: flags.model.map(str::to_owned),
            },
            session: Layer::default(),
        };
        if let Some(default) = &parts.config.model.default {
            pmt_providers::retirement::check_reference(
                &parts.config,
                &parts.registry,
                default,
                parts.config.model.provider.as_deref(),
            )?;
        }
        let requested = requested(&selection, &parts.config.model);
        if requested.provider == "auto" && requested.model.is_none() && parts.ad_hoc.is_none() {
            return Err(Error::new(&codes::MODEL_NOT_CONFIGURED, NO_MODEL));
        }
        let route = parts
            .resolver()
            .resolve(&requested, &mut Trace::default())
            .await?;
        Ok(target_of(&route))
    }

    /// The port of `target` without the fallback chain (a delegated child
    /// that pinned its model).
    ///
    /// # Errors
    ///
    /// Resolution errors of the target.
    pub async fn port_pinned(&self, target: &Target) -> Result<Arc<ProviderModel>, Error> {
        self.maker().port_pinned(target).await
    }

    /// The route `model` (`[<provider>/]<model>`, as `/model` reads it)
    /// names, resolved: what a delegated child pinning a model runs on.
    ///
    /// # Errors
    ///
    /// `model.invalid_ref` when it does not parse; resolution errors.
    pub async fn pinned_target(&self, model: &str) -> Result<Target, Error> {
        let parts = &self.setup;
        let target =
            pmt_providers::target::parse(model, &parts.config.model, &parts.registry, None)?;
        let route = parts
            .resolver()
            .resolve_target(&target, &mut Trace::default())
            .await?;
        Ok(Target {
            provider: route.provider,
            model: Some(route.model),
            ..target
        })
    }

    /// The route `-m` names on its own, with the `[<provider>/]<model>`
    /// grammar of `/model`: an alias or `<registered provider>/<model>`
    /// picks the provider too. A bare model, or a prefix that is not a
    /// provider (an aggregator's `vendor/model` id), stays a model of the
    /// configured provider; with `--provider` the model is taken verbatim.
    fn flag_target(&self, flags: &RouteFlags<'_>) -> Option<Target> {
        if flags.provider.is_some() {
            return None;
        }
        let parts = &self.setup;
        pmt_providers::target::parse(flags.model?, &parts.config.model, &parts.registry, None).ok()
    }

    /// The `compression` auxiliary route of the session served by
    /// `model`, as the summarizer of compaction and `session_search`.
    pub(crate) fn summarizer(
        &self,
        model: &Arc<SwitchableModel>,
        session: pmt_core::id::SessionId,
    ) -> Arc<super::summarize::RouteSummarizer> {
        Arc::new(super::summarize::RouteSummarizer {
            parts: Arc::clone(&self.setup),
            aux: Arc::clone(&self.aux),
            model: Arc::clone(model),
            session,
        })
    }

    /// The session's approval guardian on its `approval` auxiliary route.
    pub(crate) fn guardian(
        &self,
        model: &Arc<SwitchableModel>,
        session: pmt_core::id::SessionId,
    ) -> Arc<super::guardian::RouteGuardian> {
        Arc::new(super::guardian::RouteGuardian {
            parts: Arc::clone(&self.setup),
            aux: Arc::clone(&self.aux),
            model: Arc::clone(model),
            session,
        })
    }

    /// The port of `target`, with the profile's fallback chain.
    ///
    /// # Errors
    ///
    /// Resolution errors of the target or the chain.
    pub async fn port(&self, target: &Target) -> Result<Arc<ProviderModel>, Error> {
        self.maker().port(target).await
    }

    /// What builds ports of this profile's routes, owned (a plugin's
    /// model override is resolved at call time).
    pub(crate) fn maker(&self) -> PortMaker {
        PortMaker {
            setup: Arc::clone(&self.setup),
            caller: self.caller.clone(),
        }
    }

    /// A resolver over the profile (for `/model`).
    pub(crate) fn resolver(&self) -> Resolver<'_> {
        self.setup.resolver()
    }

    /// The profile's model catalog.
    pub(crate) fn catalog(&self) -> &Catalog {
        &self.setup.catalog
    }

    /// The `vision` auxiliary route of a session on `main` as a describer;
    /// `None` when no route answers.
    ///
    /// # Errors
    ///
    /// The resolution error of a pinned `[auxiliary.vision]` route.
    pub(crate) async fn describer(
        &self,
        main: &Route,
        session: pmt_core::id::SessionId,
    ) -> Result<Option<AuxDescriber>, Error> {
        let resolver = self.resolver();
        let ctx = AuxContext {
            resolver: &resolver,
            main: Some(main),
            health: &*self.setup.health,
        };
        let Some(route) = auxiliary::resolve(AuxTask::Vision, &AuxArgs::default(), &ctx).await?
        else {
            return Ok(None);
        };
        let context = CallContext::new(session, &AuxTask::Vision.purpose());
        Ok(Some(AuxDescriber::new(
            Arc::clone(&self.aux),
            route,
            context,
        )))
    }
}

/// Confirms a move from a subscription to a metered credential on the
/// session's yes/no line.
pub(crate) struct AskMetered(pub(crate) Arc<dyn pmt_memory::session::Approver>);

impl MeteredConfirm for AskMetered {
    fn confirm<'a>(
        &'a self,
        from: &'a str,
        to: &'a str,
    ) -> pmt_providers::resolve::BoxFuture<'a, bool> {
        Box::pin(async move {
            let question = format!(
                "The subscription route failed; continue on {to} instead of {from} (pay per token)? [y/N]"
            );
            self.0.ask(&question).await.unwrap_or(false)
        })
    }
}

/// The target that reproduces `route`.
#[must_use]
pub fn target_of(route: &Route) -> Target {
    Target {
        provider: route.provider.clone(),
        model: Some(route.model.clone()),
        ..Target::default()
    }
}

/// The session's model port; `/model` swaps the route between turns and
/// `/reasoning` sets the session's effort, which follows a switch.
pub struct SwitchableModel {
    current: RwLock<(Target, Arc<ProviderModel>)>,
    session: std::sync::Mutex<SessionModel>,
    watch: std::sync::Mutex<Option<CredentialWatch>>,
    /// The route a `/model --once` switch replaced, put back when the turn
    /// that used the switch ends.
    restore: std::sync::Mutex<Option<Restore>>,
    /// The session's `*_api_request` hooks.
    api_hooks: std::sync::OnceLock<super::api_hooks::ApiHooks>,
}

/// The route to go back to after a `--once` turn.
struct Restore {
    route: (Target, Arc<ProviderModel>),
    /// Set when a turn starts on the `--once` route: that turn's end
    /// restores `route`, not the end of a turn already running.
    used: bool,
}

impl SwitchableModel {
    /// A port over `model`, which serves `target`.
    #[must_use]
    pub fn new(target: Target, model: Arc<ProviderModel>) -> Self {
        Self {
            current: RwLock::new((target, model)),
            session: std::sync::Mutex::default(),
            watch: std::sync::Mutex::default(),
            restore: std::sync::Mutex::default(),
            api_hooks: std::sync::OnceLock::new(),
        }
    }

    /// Fires `hooks` around every call from now on.
    pub fn observe_api(&self, hooks: super::api_hooks::ApiHooks) {
        let _ = self.api_hooks.set(hooks);
    }

    /// Calls `watch` with the credential of every route this port sends on,
    /// across `/model` switches.
    pub fn watch_credentials(&self, watch: CredentialWatch) {
        self.model().watch_credentials(Arc::clone(&watch));
        *self.watch.lock().unwrap_or_else(PoisonError::into_inner) = Some(watch);
    }

    /// The target the port serves.
    #[must_use]
    pub fn target(&self) -> Target {
        self.read().0.clone()
    }

    /// Serves `model` (for `target`) from the next call until the session
    /// switches again, keeping the session's reasoning level; a pending
    /// `--once` switch is dropped.
    pub fn switch(&self, target: Target, model: Arc<ProviderModel>) {
        *self.restore() = None;
        self.replace(target, model);
    }

    /// Serves `model` (for `target`) for the next turn only: when that turn
    /// ends, [`Self::end_turn`] puts back the route (and its reasoning
    /// level) the session had before the first pending `--once` switch.
    pub fn switch_once(&self, target: Target, model: Arc<ProviderModel>) {
        let mut restore = self.restore();
        if restore.is_none() {
            let previous = self.read().clone();
            *restore = Some(Restore {
                route: previous,
                used: false,
            });
        }
        drop(restore);
        self.replace(target, model);
    }

    /// Ends a turn: after a turn on a `--once` route, the session goes back
    /// to the route it had before and its target is returned.
    pub fn end_turn(&self) -> Option<Target> {
        let mut restore = self.restore();
        if !restore.as_ref().is_some_and(|r| r.used) {
            return None;
        }
        let (target, model) = restore.take()?.route;
        drop(restore);
        *self.current.write().unwrap_or_else(PoisonError::into_inner) = (target.clone(), model);
        Some(target)
    }

    fn restore(&self) -> std::sync::MutexGuard<'_, Option<Restore>> {
        self.restore.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn replace(&self, target: Target, model: Arc<ProviderModel>) {
        model.set_reasoning(self.model().reasoning());
        if let Some(watch) = self
            .watch
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
        {
            model.watch_credentials(watch);
        }
        *self.current.write().unwrap_or_else(PoisonError::into_inner) = (target, model);
    }

    /// The session's `/model` and `/reasoning` choices.
    #[must_use]
    pub fn session(&self) -> SessionModel {
        self.session
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Keeps `session` and sends its reasoning level from the next request.
    pub fn set_session(&self, session: SessionModel) {
        self.model().set_reasoning(session.reasoning());
        *self.session.lock().unwrap_or_else(PoisonError::into_inner) = session;
    }

    /// Sends `level` from the next request without changing the session
    /// level (a `/model --reasoning` choice).
    pub fn set_reasoning(&self, level: Option<ReasoningLevel>) {
        self.model().set_reasoning(level);
    }

    fn model(&self) -> Arc<ProviderModel> {
        Arc::clone(&self.read().1)
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, (Target, Arc<ProviderModel>)> {
        self.current.read().unwrap_or_else(PoisonError::into_inner)
    }
}

impl ModelPort for SwitchableModel {
    fn route(&self) -> BoxFuture<'_, RouteInfo> {
        let model = self.model();
        Box::pin(async move { model.route().await })
    }

    fn start_turn(&self) -> BoxFuture<'_, ()> {
        if let Some(restore) = self.restore().as_mut() {
            restore.used = true;
        }
        let model = self.model();
        Box::pin(async move { model.start_turn().await })
    }

    fn call<'a>(
        &'a self,
        request: &'a Request,
        ctx: &'a CallContext,
        deltas: &'a dyn DeltaSink,
    ) -> BoxFuture<'a, Result<ModelReply, ModelFailure>> {
        let model = self.model();
        let target = self.target();
        Box::pin(async move {
            let hooks = self.api_hooks.get();
            if let Some(hooks) = hooks {
                hooks.before(&target, request).await;
            }
            let result = model.call(request, ctx, deltas).await;
            if let Some(hooks) = hooks {
                hooks.after(&target, &result).await;
            }
            result
        })
    }
}

/// The `subscription` field of a session's status (spec: model-providers,
/// Subscription plan and quota visibility): the stored plan and quota of
/// the subscription `route` bills, `quota: not reported by provider` for a
/// subscription without quota data, `None` for other billing sources.
#[must_use]
pub fn subscription_status(
    store: &Store,
    route: &pmt_agent::turn::model::RouteInfo,
) -> Option<pmt_rpc::catalog::contracts::SubscriptionStatus> {
    use pmt_rpc::catalog::contracts::{QuotaStatus, QuotaWindow, SubscriptionStatus};
    let (reported, quota) =
        pmt_providers::quota::for_session(store, &route.provider, &route.billing_source)?;
    let text = match (&quota, reported) {
        (Some(quota), _) => format!("quota: {}", quota.describe()),
        (None, true) => "quota: not fetched yet".to_owned(),
        (None, false) => pmt_providers::quota::NOT_REPORTED.to_owned(),
    };
    let (plan, windows) = quota.map_or_else(Default::default, |q| (q.plan, q.windows));
    Some(SubscriptionStatus {
        quota: QuotaStatus {
            reported,
            plan,
            windows: windows
                .into_iter()
                .map(|w| QuotaWindow {
                    name: w.name,
                    used_percent: w.used_percent,
                    resets_at: w.resets_at,
                })
                .collect(),
            text,
        },
    })
}

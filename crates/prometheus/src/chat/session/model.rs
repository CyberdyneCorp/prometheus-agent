//! The session's model (spec: model-providers; cli, Stable exit codes):
//! the route a new session resolves from the chat flags and the profile,
//! the mock provider when asked for, and a port `/model` can point at
//! another route between turns.

use std::path::Path;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use pmt_agent::turn::model::{
    BoxFuture, DeltaSink, ModelFailure, ModelPort, ModelReply, ProviderModel, ProviderSetup,
    RouteInfo,
};
use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_obs::usage::{Prices, UsageRecorder};
use pmt_providers::ModelCaller;
use pmt_providers::ProviderRegistry;
use pmt_providers::auth::Accounts;
use pmt_providers::cost::CallContext;
use pmt_providers::fallback::Healthy;
use pmt_providers::metadata::Catalog;
use pmt_providers::mock::Mock;
use pmt_providers::request::Request;
use pmt_providers::resolve::{
    AdHoc, Layer, Resolver, Route, Selection, Target, TcpProbe, Trace, requested,
};
use pmt_store::Store;

/// The exact message of a profile with nothing to talk to.
pub const NO_MODEL: &str = "no model configured; run: prometheus setup model";

/// What builds the session's routes: the profile's providers, catalog,
/// credentials and the mock provider.
pub struct Models {
    setup: Arc<ProviderSetupParts>,
    caller: ModelCaller,
}

/// The shareable parts of a [`ProviderSetup`].
struct ProviderSetupParts {
    config: Arc<pmt_config::Config>,
    registry: Arc<ProviderRegistry>,
    secrets: pmt_config::SecretsHandle,
    accounts: Arc<Accounts>,
    catalog: Arc<Catalog>,
    ad_hoc: Option<AdHoc>,
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
            health: Arc::new(Healthy),
            ad_hoc: self.ad_hoc.clone(),
        }
    }

    fn resolver(&self) -> Resolver<'_> {
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
        let caller = ModelCaller::new(crate::model_cmd::client(config)?)
            .with_mock(mock)
            .with_recorder(Arc::new(UsageRecorder::new(Arc::clone(store), prices)))
            .with_credentials(Arc::clone(&accounts) as _);
        Ok(Self {
            setup: Arc::new(ProviderSetupParts {
                config: Arc::new(config.clone()),
                registry: Arc::new(registry),
                secrets: scope.secrets().clone(),
                accounts,
                catalog,
                ad_hoc,
            }),
            caller,
        })
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
        let selection = Selection {
            flag: Layer {
                provider: flags.provider.map(str::to_owned),
                model: flags.model.map(str::to_owned),
            },
            session: Layer::default(),
        };
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

    /// The port of `target`, with the profile's fallback chain.
    ///
    /// # Errors
    ///
    /// Resolution errors of the target or the chain.
    pub async fn port(&self, target: &Target) -> Result<Arc<ProviderModel>, Error> {
        let model = ProviderModel::start(self.setup.setup(), self.caller.clone(), target).await?;
        Ok(Arc::new(model))
    }

    /// A resolver over the profile (for `/model`).
    pub(crate) fn resolver(&self) -> Resolver<'_> {
        self.setup.resolver()
    }

    /// The profile's model catalog.
    pub(crate) fn catalog(&self) -> &Catalog {
        &self.setup.catalog
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

/// The session's model port; `/model` swaps the route between turns.
pub struct SwitchableModel {
    current: RwLock<(Target, Arc<ProviderModel>)>,
}

impl SwitchableModel {
    /// A port over `model`, which serves `target`.
    #[must_use]
    pub fn new(target: Target, model: Arc<ProviderModel>) -> Self {
        Self {
            current: RwLock::new((target, model)),
        }
    }

    /// The target the port serves.
    #[must_use]
    pub fn target(&self) -> Target {
        self.read().0.clone()
    }

    /// Serves `model` (for `target`) from the next call.
    pub fn switch(&self, target: Target, model: Arc<ProviderModel>) {
        *self.current.write().unwrap_or_else(PoisonError::into_inner) = (target, model);
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
        Box::pin(async move { model.call(request, ctx, deltas).await })
    }
}

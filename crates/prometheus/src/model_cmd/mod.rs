//! `prometheus model` (spec: model-providers, prometheus model command;
//! Deterministic explainable resolution; Built-in provider catalog): the
//! interactive picker, `set`, `list`, `info` and `explain`. Every command
//! resolves routes for the selected profile exactly as a new session
//! would, and never prints a secret value.

pub(crate) mod explain;
mod info;
mod list;
pub(crate) mod picker;
mod set;

use std::future::Future;
use std::time::Duration;

use pmt_config::{Config, ProfileScope};
use pmt_core::error::{Error, codes};
use pmt_providers::ProviderRegistry;
use pmt_providers::auth::Accounts;
use pmt_providers::capability::{self, Needs};
use pmt_providers::metadata::{Catalog, ModelMetadata};
use pmt_providers::resolve::{AdHoc, Resolver, Route, Target, TcpProbe, Trace};

use crate::cli::ModelCommand;
use crate::context::Context;

/// The terminal-required message of the bare picker.
pub const REQUIRES_TERMINAL: &str =
    "prometheus model requires a terminal; use 'prometheus model set <provider>/<model>'";

static PROBE: TcpProbe = TcpProbe {
    timeout: Duration::from_millis(500),
};

/// Runs one `model` subcommand (`None`: the picker) and returns its exit
/// code.
///
/// # Errors
///
/// The command's failure; the caller prints it and exits with its code.
pub fn run(command: Option<&ModelCommand>, ctx: &Context<'_>) -> Result<u8, Error> {
    match command {
        None => picker::run(ctx),
        Some(ModelCommand::Explain {
            provider,
            model,
            endpoint,
        }) => explain::run(ctx, provider.clone(), model.clone(), endpoint),
        Some(ModelCommand::Set {
            model,
            wire_format,
            base_url,
        }) => set::run(ctx, model, *wire_format, base_url.as_deref()),
        Some(ModelCommand::List {
            provider,
            providers,
            refresh,
        }) => list::run(ctx, provider.as_deref(), *providers, *refresh),
        Some(ModelCommand::Info) => info::run(ctx),
    }
}

/// One profile's providers and model catalog.
pub(crate) struct Models {
    pub scope: ProfileScope,
    pub registry: ProviderRegistry,
    pub catalog: Catalog,
    /// The profile's credentials (resolution uses them like a session
    /// would).
    pub accounts: Accounts,
}

impl Models {
    /// The selected profile's registry and catalog.
    pub(crate) fn open(ctx: &Context<'_>) -> Result<Self, Error> {
        let scope = ctx.scope()?;
        let user_home = ctx.launch.user_home().map(std::path::PathBuf::as_path);
        Self::for_config(scope.clone(), scope.config(), user_home)
    }

    /// The registry and catalog of `config` (a proposed config for
    /// validating a change before it is written) for the user whose home
    /// is `user_home` (borrowed logins).
    pub(crate) fn for_config(
        scope: ProfileScope,
        config: &Config,
        user_home: Option<&std::path::Path>,
    ) -> Result<Self, Error> {
        let registry = ProviderRegistry::for_profile(config, Vec::new())?;
        let catalog = Catalog::new(config, Some(scope.path("cache/models")));
        let accounts = crate::accounts::open(&scope, config, &registry, user_home)?;
        Ok(Self {
            scope,
            registry,
            catalog,
            accounts,
        })
    }

    /// A resolver over this profile's secrets.
    pub(crate) fn resolver<'a>(
        &'a self,
        config: &'a Config,
        ad_hoc: Option<AdHoc>,
    ) -> Resolver<'a> {
        Resolver {
            registry: &self.registry,
            config,
            secrets: self.scope.secrets(),
            extras: &self.accounts,
            probe: &PROBE,
            ad_hoc,
        }
    }
}

/// Runs `future` on a fresh current-thread runtime.
/// `prometheus model set <reference>` without flags (the setup wizard).
///
/// # Errors
///
/// As `model set`.
pub(crate) fn set_model(ctx: &Context<'_>, reference: &str) -> Result<u8, Error> {
    set::run(ctx, reference, None, None)
}

pub(crate) fn block_on<F: Future>(future: F) -> Result<F::Output, Error> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::new(&codes::OUTPUT_IO, format!("cannot start the runtime: {e}")))?;
    Ok(runtime.block_on(future))
}

/// The profile's outbound HTTP client (listings, probes, models.dev).
pub(crate) fn client(config: &Config) -> Result<reqwest::Client, Error> {
    let policy = pmt_net::OutboundPolicy {
        force_ipv4: config.network.force_ipv4,
        extra_ca_file: config.network.extra_ca_file.as_ref().map(Into::into),
    };
    pmt_net::client_builder(&policy)?
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| {
            Error::new(
                &codes::NET_CLIENT_INIT,
                format!("cannot build the HTTP client: {e}"),
            )
        })
}

/// Validates `target` as a session route: resolution with its own
/// credential, tool support (sessions offer tools) and the minimum
/// context window.
pub(crate) async fn validate(
    resolver: &Resolver<'_>,
    catalog: &Catalog,
    target: &Target,
) -> Result<(Route, ModelMetadata), Error> {
    let route = resolver
        .resolve_target(target, &mut Trace::default())
        .await?;
    let (meta, _) = catalog.metadata_at(&route.provider, &route.model, Some(&route.base_url));
    let needs = Needs {
        tools: true,
        ..Needs::default()
    };
    capability::check(&route.label(), &meta, &needs)?;
    capability::check_min_context(
        &route.label(),
        &meta,
        resolver.config.model.min_context_window,
    )?;
    Ok((route, meta))
}

//! The daemon's messaging gateway setup (spec: messaging-gateway): the
//! adapters this build has, the daemon-wide settings from `daemon.toml`
//! (all-adapters-down policy, relay connector), and the launch profile's
//! shutdown, agent-cache and health-export settings. First-party platform
//! adapters register their factories here as they land
//! (platform-adapters); an enabled platform without one is reported
//! `fatal` with `unknown_platform`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use pmt_config::{DaemonConfig, ProfileScope};
use pmt_core::error::Error;
use pmt_gateway::messaging::health::HealthOptions;
use pmt_gateway::messaging::{AdapterFactory, Agents as _, Options, RelayOptions, RelaySource};

use super::host::DaemonHost;
use crate::context::Context;

/// The gateway's options for this daemon.
///
/// # Errors
///
/// The relay's credential is missing from the launch profile's secrets.
pub(super) fn options(
    home: &Path,
    config: &DaemonConfig,
    host: &Arc<DaemonHost>,
    launch_profile: &str,
    ctx: &Context<'_>,
) -> Result<Options, Error> {
    let mut options = Options::new(home);
    options.factories = factories(ctx);
    options.plugin_factory = Some(crate::plugins_host::adapter::factory());
    let scopes = Arc::clone(host);
    options
        .hooks
        .set_bridge(Arc::new(crate::plugins_host::gateway::Bridge::new(
            Arc::new(move |profile: &str| scopes.scope(profile)),
        )));
    options.on_all_adapters_down = config.gateway.on_all_adapters_down;
    // `DaemonConfig::load` already refused a bad address.
    if let Ok(listen) = config.gateway.http.listen_addr() {
        options.http = pmt_gateway::messaging::HttpOptions {
            listen,
            public: config.gateway.http.public,
        };
    }
    options.locks_dir =
        pmt_core::inventory::user_runtime_dir(ctx.launch).map(|dir| dir.join("locks"));
    if let Some(scope) = host.scope(launch_profile) {
        let gateway = &scope.config().gateway;
        options.drain_timeout = Duration::from_secs(gateway.shutdown.drain_timeout_secs);
        options.agent_cache = gateway.agent_cache.clone();
        options.health = health(&scope);
        options.relay = relay(config, &scope)?;
    }
    let (home, host, launch) = (
        home.to_path_buf(),
        Arc::clone(host),
        launch_profile.to_owned(),
    );
    let source: RelaySource = Arc::new(move || {
        let config = DaemonConfig::load(&home)?;
        match host.scope(&launch) {
            Some(scope) => relay(&config, &scope),
            None => Ok(None),
        }
    });
    options.relay_source = Some(source);
    Ok(options)
}

fn extra_ca(scope: &ProfileScope) -> Option<PathBuf> {
    scope
        .config()
        .network
        .extra_ca_file
        .as_deref()
        .map(PathBuf::from)
}

/// `observability.gateway_health` of the launch profile, when enabled.
fn health(scope: &ProfileScope) -> Option<HealthOptions> {
    let health = &scope.config().observability.gateway_health;
    if !health.enabled {
        return None;
    }
    Some(HealthOptions {
        endpoint: health.endpoint.clone()?,
        interval: Duration::from_secs(health.interval_secs.max(1)),
        extra_ca_file: extra_ca(scope),
    })
}

/// `daemon.toml gateway.relay` with its credential from the launch
/// profile's secrets store.
fn relay(config: &DaemonConfig, scope: &ProfileScope) -> Result<Option<RelayOptions>, Error> {
    let relay = &config.gateway.relay;
    let Some(url) = relay.url.clone() else {
        return Ok(None);
    };
    let token = scope.secrets().resolve(&relay.token_secret.0)?;
    Ok(Some(RelayOptions {
        url,
        token,
        gateway_id: relay.gateway_id.clone(),
        allow_direct_platforms: relay.allow_direct_platforms.clone(),
        default_profile: scope.name().as_str().to_owned(),
        extra_ca_file: extra_ca(scope),
    }))
}

/// The adapter factories of this build, by platform name.
pub(super) fn factories(ctx: &Context<'_>) -> BTreeMap<String, AdapterFactory> {
    // Release builds have no test adapter to insert.
    #[cfg_attr(not(debug_assertions), allow(unused_mut))]
    let mut factories: BTreeMap<String, AdapterFactory> = BTreeMap::from([
        (
            pmt_adapters_telegram::PLATFORM.to_owned(),
            pmt_adapters_telegram::factory(),
        ),
        (
            pmt_adapters_discord::PLATFORM.to_owned(),
            pmt_adapters_discord::factory(),
        ),
        (
            pmt_adapters_slack::PLATFORM.to_owned(),
            pmt_adapters_slack::factory(),
        ),
        (
            pmt_adapters_email::PLATFORM.to_owned(),
            pmt_adapters_email::factory(),
        ),
        (
            pmt_adapters_matrix::PLATFORM.to_owned(),
            pmt_adapters_matrix::factory(),
        ),
        (
            pmt_adapters_signal::PLATFORM.to_owned(),
            pmt_adapters_signal::factory(),
        ),
        (
            pmt_adapters_whatsapp::PLATFORM.to_owned(),
            pmt_adapters_whatsapp::factory(),
        ),
        (
            pmt_adapters_webhook::PLATFORM.to_owned(),
            pmt_adapters_webhook::factory(),
        ),
    ]);
    #[cfg(debug_assertions)]
    if let Some(dir) = ctx.launch.test_adapter_dir() {
        factories.insert(
            super::test_adapter::PLATFORM.to_owned(),
            super::test_adapter::factory(dir.clone()),
        );
    }
    #[cfg(not(debug_assertions))]
    let _ = ctx;
    factories
}

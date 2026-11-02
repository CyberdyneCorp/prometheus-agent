//! The daemon's messaging gateway setup (spec: messaging-gateway): the
//! adapters this build has, the daemon-wide all-adapters-down policy from
//! `daemon.toml`, and the launch profile's shutdown and agent-cache
//! settings. First-party platform adapters register their factories here
//! as they land (platform-adapters); until then an enabled platform is
//! reported `fatal` with `unknown_platform`.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use pmt_config::DaemonConfig;
use pmt_gateway::messaging::{AdapterFactory, Agents as _, Options};

use super::host::DaemonHost;
use crate::context::Context;

/// The gateway's options for this daemon.
pub(super) fn options(
    home: &Path,
    config: &DaemonConfig,
    host: &DaemonHost,
    launch_profile: &str,
    ctx: &Context<'_>,
) -> Options {
    let mut options = Options::new(home);
    options.factories = factories(ctx);
    options.on_all_adapters_down = config.gateway.on_all_adapters_down;
    if let Some(scope) = host.scope(launch_profile) {
        let gateway = &scope.config().gateway;
        options.drain_timeout = Duration::from_secs(gateway.shutdown.drain_timeout_secs);
        options.agent_cache = gateway.agent_cache.clone();
    }
    options
}

/// The adapter factories of this build, by platform name.
fn factories(ctx: &Context<'_>) -> BTreeMap<String, AdapterFactory> {
    let mut factories: BTreeMap<String, AdapterFactory> = BTreeMap::new();
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

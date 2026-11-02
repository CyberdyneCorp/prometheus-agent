//! `prometheus daemon run` (spec: messaging-gateway, One gateway daemon per
//! home; workspace-architecture, One daemon per Prometheus home). The
//! home's writer lock is taken before anything else, so a second daemon
//! exits 75 without opening any `state.db`; then `daemon.toml` and the
//! served profiles are loaded (spec: configuration-and-profiles, Served
//! profile set): the launch profile, whose invalid configuration exits 78,
//! and with its `gateway.multiplex = true` every other profile of the home
//! except those with `gateway.standalone = true` (one whose configuration
//! is invalid is logged and skipped). The daemon serves until it is
//! stopped (exit 0) or restarted (exit 75, so a supervisor starts the new
//! binary), reloading each profile's `config.toml` when it changes.
//!
//! Logging (spec: observability, Structured logs): the daemon's own records
//! are the `gateway` component (`<home>/logs/gateway.log`), work on a
//! profile's session is recorded in that profile's `logs/`, and every
//! served profile registers its redactor and logs its startup security
//! warnings; the launch profile's `logging.level` applies daemon-wide.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use pmt_config::{DaemonConfig, Launch, Overrides, ProfileScope};
use pmt_core::error::Error;
use pmt_core::runtime::{Role, WriterLock};
use pmt_gateway::messaging::Gateway;
use pmt_gateway::{Exit, Identity, Server, ServerOptions};
use tracing::Instrument as _;

use super::host::DaemonHost;
use crate::context::Context;

/// Runs the daemon in the foreground; returns its exit status.
///
/// # Errors
///
/// `home.locked` (75) when a daemon or chat holds the home, `config.*`
/// (78) for `daemon.toml` or when no profile loads, listener errors.
pub(super) fn run(home: &Path, ctx: &Context<'_>, replace_locks: bool) -> Result<u8, Error> {
    let span = tracing::info_span!("daemon", component = "gateway");
    let _daemon = span.enter();
    let layout = ctx.home()?;
    let names: Vec<String> = layout
        .profiles()?
        .iter()
        .map(|p| p.as_str().to_owned())
        .collect();
    let lock = WriterLock::acquire(home, Role::Daemon, names)?;
    let config = DaemonConfig::load(home)?;
    let launch_profile = layout.select_profile(ctx.profile)?;
    let scopes = served_profiles(&layout, &launch_profile, ctx)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| {
            Error::new(
                &pmt_core::error::codes::OUTPUT_IO,
                format!("cannot start the runtime: {e}"),
            )
        })?;
    let exit = runtime.block_on(
        async {
            let host = Arc::new(DaemonHost::new(
                scopes,
                ctx.launch.clone(),
                ctx.set.to_vec(),
                launch_profile.as_str().to_owned(),
            )?);
            let mut options = ServerOptions::new(identity(), host.names());
            options.rpc = config.daemon.rpc.clone();
            options.idle_session_close = Duration::from_secs(config.daemon.idle_session_close_secs);
            options.crash_reporter = Some(Arc::new(crate::startup::crash_reporter(
                Some(&home.to_path_buf()),
                ctx.launch,
            )));
            let watcher = tokio::spawn(super::host::watch(Arc::clone(&host)));
            let server = Server::new(options, Arc::clone(&host) as Arc<dyn pmt_gateway::Host>)?;
            let mut messaging =
                super::messaging::options(home, &config, &host, launch_profile.as_str(), ctx)?;
            messaging.replace_locks = replace_locks;
            let agents = Arc::clone(&host) as Arc<dyn pmt_gateway::messaging::Agents>;
            match Gateway::start(Arc::clone(&server), agents, messaging).await {
                Ok(gateway) => host.set_gateway(gateway),
                Err(err) => {
                    // The daemon cannot tell whether its last stop was clean.
                    tracing::error!("{err}");
                    eprint!("{}", crate::output::render_error(&err, false));
                    watcher.abort();
                    return Ok(pmt_core::exit::FAILURE);
                }
            }
            super::inventory::register(
                home,
                ctx.launch,
                enabled_adapters(&scopes_of(&server, &host)),
            );
            let drain = Duration::from_secs(config.daemon.restart_drain_secs);
            let exit = pmt_gateway::run(&lock, home, server, drain, shutdown_signal()).await;
            super::inventory::unregister(home, ctx.launch);
            watcher.abort();
            exit.map(exit_code)
        }
        .in_current_span(),
    )?;
    // Turns still running past the drain are abandoned with the runtime.
    runtime.shutdown_timeout(Duration::from_secs(1));
    Ok(exit)
}

/// The served profiles' scopes.
fn scopes_of(server: &Server, host: &DaemonHost) -> Vec<ProfileScope> {
    use pmt_gateway::messaging::Agents as _;
    server
        .profiles()
        .iter()
        .filter_map(|name| host.scope(name))
        .collect()
}

/// The platforms each served profile enables, for the daemon registry.
fn enabled_adapters(scopes: &[ProfileScope]) -> std::collections::BTreeMap<String, Vec<String>> {
    scopes
        .iter()
        .map(|scope| {
            let platforms = scope
                .config()
                .gateway
                .platforms
                .iter()
                .filter(|(_, t)| t.enabled)
                .map(|(name, _)| name.clone())
                .collect();
            (scope.name().as_str().to_owned(), platforms)
        })
        .collect()
}

/// The process exit status of a daemon stop.
fn exit_code(exit: Exit) -> u8 {
    match exit {
        Exit::Stopped => 0,
        Exit::Restart => pmt_core::exit::TEMPORARY_FAILURE,
        Exit::AdaptersDown => pmt_core::exit::CONFIG,
    }
}

pub(super) fn identity() -> Identity {
    use crate::build_info;
    Identity {
        version: build_info::VERSION.to_owned(),
        build_id: build_info::BUILD_ID.to_owned(),
        target: build_info::TARGET.to_owned(),
        tier: build_info::tier(build_info::TARGET)
            .map_or_else(|| "unsupported".to_owned(), |t| format!("tier{t}")),
    }
}

/// The served profiles: the launch profile (it alone sees the launch
/// environment's secrets), and with its `gateway.multiplex` every other
/// profile that loads and is not `gateway.standalone`. Profile
/// directories are only read here, never created.
fn served_profiles(
    layout: &pmt_config::Home,
    launch_profile: &pmt_config::ProfileName,
    ctx: &Context<'_>,
) -> Result<Vec<ProfileScope>, Error> {
    let overrides = Overrides::parse(ctx.set)?;
    let open = |name: &pmt_config::ProfileName, launch| {
        layout
            .existing_profile(name)
            .and_then(|dir| ProfileScope::open(dir, &overrides, launch))
    };
    let first = open(launch_profile, Launch::Profile(ctx.launch))?;
    if let Some(handle) = pmt_obs::logs::handle() {
        handle.set_level(first.config().logging.level);
    }
    crate::startup::register_profile(&first);
    if !first.config().gateway.multiplex {
        return Ok(vec![first]);
    }
    let mut scopes = vec![first];
    for name in layout.profiles()? {
        if &name == launch_profile {
            continue;
        }
        match open(&name, Launch::Other) {
            Ok(scope) if scope.config().gateway.standalone => {
                tracing::info!(profile = name.as_str(), "not served: gateway.standalone");
            }
            Ok(scope) => {
                crate::startup::register_profile(&scope);
                scopes.push(scope);
            }
            Err(err) => tracing::error!(profile = name.as_str(), "profile not served: {err}"),
        }
    }
    Ok(scopes)
}

/// SIGINT, or SIGTERM on Unix: a graceful stop.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

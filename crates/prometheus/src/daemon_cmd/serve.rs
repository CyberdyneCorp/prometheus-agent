//! `prometheus daemon run` (spec: messaging-gateway, One gateway daemon per
//! home; workspace-architecture, One daemon per Prometheus home). The
//! home's writer lock is taken before anything else, so a second daemon
//! exits 75 without opening any `state.db`; then `daemon.toml` and every
//! profile are loaded (a profile whose configuration is invalid is logged
//! and skipped; when none is left the daemon exits 78), and the daemon
//! serves until it is stopped (exit 0) or restarted (exit 75, so a
//! supervisor starts the new binary).

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use pmt_config::{DaemonConfig, Launch, Overrides, ProfileScope};
use pmt_core::error::Error;
use pmt_core::runtime::{Role, WriterLock};
use pmt_gateway::{Exit, Identity, Server, ServerOptions};

use super::host::DaemonHost;
use crate::context::Context;

/// Runs the daemon in the foreground; returns its exit status.
///
/// # Errors
///
/// `home.locked` (75) when a daemon or chat holds the home, `config.*`
/// (78) for `daemon.toml` or when no profile loads, listener errors.
pub(super) fn run(home: &Path, ctx: &Context<'_>) -> Result<u8, Error> {
    let layout = ctx.home()?;
    let names: Vec<String> = layout
        .profiles()?
        .iter()
        .map(|p| p.as_str().to_owned())
        .collect();
    let lock = WriterLock::acquire(home, Role::Daemon, names)?;
    let config = DaemonConfig::load(home)?;
    let launch_profile = layout.select_profile(ctx.profile).ok();
    let scopes = load_profiles(&layout, launch_profile.as_ref(), ctx)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| {
            Error::new(
                &pmt_core::error::codes::OUTPUT_IO,
                format!("cannot start the runtime: {e}"),
            )
        })?;
    let exit = runtime.block_on(async {
        let host = DaemonHost::new(scopes, ctx.launch.clone())?;
        let mut options = ServerOptions::new(identity(), host.names());
        options.rpc = config.daemon.rpc.clone();
        let server = Server::new(options, Arc::new(host))?;
        let drain = Duration::from_secs(config.daemon.restart_drain_secs);
        pmt_gateway::run(&lock, home, server, drain, shutdown_signal()).await
    })?;
    // Turns still running past the drain are abandoned with the runtime.
    runtime.shutdown_timeout(Duration::from_secs(1));
    Ok(match exit {
        Exit::Stopped => 0,
        Exit::Restart => pmt_core::exit::TEMPORARY_FAILURE,
    })
}

fn identity() -> Identity {
    use crate::build_info;
    Identity {
        version: build_info::VERSION.to_owned(),
        build_id: build_info::BUILD_ID.to_owned(),
        target: build_info::TARGET.to_owned(),
        tier: build_info::tier(build_info::TARGET)
            .map_or_else(|| "unsupported".to_owned(), |t| format!("tier{t}")),
    }
}

/// Every profile that loads; the launch profile sees the launch
/// environment's secrets. With none left, the first error.
fn load_profiles(
    layout: &pmt_config::Home,
    launch_profile: Option<&pmt_config::ProfileName>,
    ctx: &Context<'_>,
) -> Result<Vec<ProfileScope>, Error> {
    let overrides = Overrides::parse(ctx.set)?;
    let mut scopes = Vec::new();
    let mut first_error = None;
    for name in layout.profiles()? {
        let launch = if Some(&name) == launch_profile {
            Launch::Profile(ctx.launch)
        } else {
            Launch::Other
        };
        let opened = layout
            .existing_profile(&name)
            .and_then(|dir| ProfileScope::open(dir, &overrides, launch));
        match opened {
            Ok(scope) => {
                crate::startup::bind_profile(&scope);
                scopes.push(scope);
            }
            Err(err) => {
                tracing::error!(profile = name.as_str(), "profile not served: {err}");
                first_error.get_or_insert(err);
            }
        }
    }
    match (scopes.is_empty(), first_error) {
        (true, Some(err)) => Err(err),
        (true, None) => Err(Error::new(
            &pmt_core::error::codes::DAEMON_NO_PROFILE,
            "the home has no profile to serve",
        )),
        _ => Ok(scopes),
    }
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

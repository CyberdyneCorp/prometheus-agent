//! The in-process gateway of `prometheus tui --standalone` (spec: tui,
//! Standalone in-process mode): it takes the home's exclusive daemon lock
//! for the TUI's lifetime and serves only that TUI (tickets go to its own
//! pid alone), with the same host as `prometheus daemon run`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use pmt_config::{DaemonConfig, ProfileScope};
use pmt_core::env::LaunchEnv;
use pmt_core::error::{Error, codes};
use pmt_core::runtime::{Role, WriterLock};
use pmt_gateway::{Exit, Server, ServerOptions};
use pmt_rpc::discovery::{DaemonRecord, Discovery, discover};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use super::host::DaemonHost;

/// A running in-process gateway; [`Standalone::stop`] ends it.
pub(crate) struct Standalone {
    stop: oneshot::Sender<()>,
    task: JoinHandle<Result<Exit, Error>>,
}

/// Takes the home's daemon lock, or `home.locked` (75) naming the daemon
/// that holds it. Nothing of the home is opened before.
///
/// # Errors
///
/// `home.locked`, `home.io`.
pub(crate) fn lock(home: &Path, profiles: Vec<String>) -> Result<WriterLock, Error> {
    WriterLock::acquire(home, Role::Daemon, profiles).map_err(|err| {
        if err.code().code != codes::HOME_LOCKED.code {
            return err;
        }
        let pid = match discover(home) {
            Ok(Discovery::Running(record)) => record.process.pid,
            Ok(Discovery::Starting(record)) => record.pid,
            _ => return err,
        };
        Error::new(
            &codes::HOME_LOCKED,
            format!(
                "{} is served by a running gateway daemon (pid {pid}); run without --standalone to attach",
                home.display()
            ),
        )
    })
}

/// What the in-process gateway serves.
pub(crate) struct Setup {
    /// The home root.
    pub(crate) home: PathBuf,
    /// The TUI's profile (the only one served).
    pub(crate) scope: ProfileScope,
    /// The launch environment.
    pub(crate) launch: LaunchEnv,
    /// The global `--set` overrides.
    pub(crate) set: Vec<String>,
}

impl Standalone {
    /// Starts the gateway under `lock` and waits for its endpoint.
    ///
    /// # Errors
    ///
    /// `daemon.toml`, store and listener errors; `daemon.start_failed`
    /// when it does not publish its endpoint.
    pub(crate) async fn start(
        lock: WriterLock,
        setup: Setup,
    ) -> Result<(Self, DaemonRecord), Error> {
        let config = DaemonConfig::load(&setup.home)?;
        let name = setup.scope.name().as_str().to_owned();
        let host = Arc::new(DaemonHost::new(
            vec![setup.scope],
            setup.launch,
            setup.set,
            name,
        )?);
        DaemonHost::adopt(&host);
        let mut options = ServerOptions::new(super::serve::identity(), host.names());
        options.rpc = config.daemon.rpc.clone();
        options.idle_session_close = Duration::from_secs(config.daemon.idle_session_close_secs);
        options.only_pid = Some(std::process::id());
        let server = Server::new(options, host)?;
        let (stop, stopped) = oneshot::channel::<()>();
        let home = setup.home.clone();
        let drain = Duration::from_secs(config.daemon.restart_drain_secs);
        // The in-process gateway's records are the `gateway` component,
        // as a daemon's are.
        let span = tracing::info_span!("daemon", component = "gateway");
        let task = pmt_core::spawn::spawn(tracing::Instrument::instrument(
            async move {
                let shutdown = async {
                    let _ = stopped.await;
                };
                pmt_gateway::run(&lock, &home, server, drain, shutdown).await
            },
            span,
        ));
        let record = wait_published(&setup.home, &task).await?;
        Ok((Self { stop, task }, record))
    }

    /// Stops the gateway (running turns get the drain time to finish).
    pub(crate) async fn stop(self) {
        let _ = self.stop.send(());
        let _ = self.task.await;
    }
}

async fn wait_published(
    home: &Path,
    task: &JoinHandle<Result<Exit, Error>>,
) -> Result<DaemonRecord, Error> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Discovery::Running(record) = discover(home)? {
            return Ok(record);
        }
        if task.is_finished() || tokio::time::Instant::now() > deadline {
            return Err(Error::new(
                &codes::DAEMON_START_FAILED,
                "the in-process gateway did not start",
            ));
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

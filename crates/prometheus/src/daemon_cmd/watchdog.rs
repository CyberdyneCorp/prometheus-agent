//! The runtime liveness watchdog (spec: observability, Runtime liveness
//! watchdog and task dump): a thread outside the async runtime hands it a
//! probe task every `daemon.watchdog.probe_interval_secs`; after
//! `max_strikes` probes in a row that did not run within the interval it
//! writes a task dump, marks `run/gateway.json` degraded with
//! `exit_reason = "runtime_liveness_watchdog"` and exits 75 so the
//! service supervisor restarts the daemon. With a systemd watchdog
//! (`daemon.systemd_watchdog_secs`, Linux) each answered probe is a
//! heartbeat; an unanswered one is not.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use pmt_config::daemon::WatchdogConfig;
use pmt_obs::taskdump::{self, RuntimeCounters};

use super::record;

/// The exit reason recorded when the watchdog stops the daemon.
pub(super) const EXIT_REASON: &str = "runtime_liveness_watchdog";

/// Stops the watchdog thread when dropped (a normal daemon stop).
pub(super) struct Guard(Arc<AtomicBool>);

impl Drop for Guard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Starts the watchdog of `runtime` (nothing when disabled).
pub(super) fn start(
    home: PathBuf,
    config: &WatchdogConfig,
    runtime: tokio::runtime::Handle,
    systemd: Option<super::systemd::Notifier>,
) -> Guard {
    let stop = Arc::new(AtomicBool::new(false));
    if config.enabled {
        let watch = Watch {
            home,
            interval: Duration::from_secs(config.probe_interval_secs),
            max_strikes: config.max_strikes,
            runtime,
            systemd,
            stop: Arc::clone(&stop),
        };
        let _ = std::thread::Builder::new()
            .name("pmt-watchdog".to_owned())
            .spawn(move || watch.run());
    }
    Guard(stop)
}

struct Watch {
    home: PathBuf,
    interval: Duration,
    max_strikes: u32,
    runtime: tokio::runtime::Handle,
    systemd: Option<super::systemd::Notifier>,
    stop: Arc<AtomicBool>,
}

impl Watch {
    fn run(self) {
        let mut strikes = 0;
        while !self.stop.load(Ordering::Relaxed) {
            let started = Instant::now();
            let (tx, rx) = std::sync::mpsc::channel();
            pmt_core::spawn::spawn_on(&self.runtime, async move {
                let _ = tx.send(());
            });
            if rx.recv_timeout(self.interval).is_ok() {
                strikes = 0;
                if let Some(systemd) = &self.systemd {
                    systemd.notify("WATCHDOG=1");
                }
                std::thread::sleep(self.interval.saturating_sub(started.elapsed()));
                continue;
            }
            if self.stop.load(Ordering::Relaxed) {
                return;
            }
            strikes += 1;
            tracing::warn!(
                strikes,
                "the async runtime did not answer a liveness probe within {:?}",
                self.interval
            );
            if strikes >= self.max_strikes {
                self.trip(strikes);
            }
        }
    }

    /// Dumps, marks the record degraded and exits 75.
    fn trip(&self, strikes: u32) -> ! {
        let reason = format!(
            "{EXIT_REASON}: {strikes} liveness probes unanswered within {:?}",
            self.interval
        );
        let counters = RuntimeCounters::of(&self.runtime);
        match taskdump::write(&self.home, &reason, Some(counters)) {
            Ok(path) => {
                tracing::error!(dump = %path.display(), "{reason}; exiting 75 for a restart");
            }
            Err(err) => tracing::error!("{reason}; the task dump failed: {err}"),
        }
        record::degraded(&self.home, EXIT_REASON);
        std::process::exit(i32::from(pmt_core::exit::TEMPORARY_FAILURE));
    }
}

/// The daemon's runtime-side liveness work until aborted: the
/// `updated_at` heartbeat, task dumps on SIGUSR2 (POSIX) and, in debug
/// builds, the stalled-runtime test seam.
pub(super) fn background(
    home: &std::path::Path,
    launch: &pmt_core::env::LaunchEnv,
) -> tokio::task::JoinHandle<()> {
    let home = home.to_path_buf();
    let stall = launch.test_stall_runtime();
    pmt_core::spawn::spawn(async move {
        if let Some(after) = stall {
            pmt_core::spawn::spawn(stall_runtime(after));
        }
        #[cfg(unix)]
        pmt_core::spawn::spawn(dump_on_sigusr2(home.clone()));
        record::heartbeat(home).await;
    })
}

/// SIGUSR2 writes a task dump; the daemon keeps serving.
#[cfg(unix)]
async fn dump_on_sigusr2(home: PathBuf) {
    use tokio::signal::unix::{SignalKind, signal};
    let Ok(mut signals) = signal(SignalKind::user_defined2()) else {
        return;
    };
    while signals.recv().await.is_some() {
        let counters = tokio::runtime::Handle::try_current()
            .ok()
            .map(|h| RuntimeCounters::of(&h));
        match taskdump::write(&home, "requested (SIGUSR2)", counters) {
            Ok(path) => tracing::info!(dump = %path.display(), "task dump written"),
            Err(err) => tracing::warn!("task dump failed: {err}"),
        }
    }
}

/// Debug-build test seam (`PMT_TEST_STALL_RUNTIME_MS`): after `after`,
/// every worker thread runs a task that never yields.
async fn stall_runtime(after: Duration) {
    tokio::time::sleep(after).await;
    let workers = tokio::runtime::Handle::current().metrics().num_workers();
    tracing::warn!("test seam: blocking all {workers} runtime workers");
    for _ in 0..=workers {
        pmt_core::spawn::spawn(async {
            loop {
                std::thread::sleep(Duration::from_hours(1));
            }
        });
    }
}

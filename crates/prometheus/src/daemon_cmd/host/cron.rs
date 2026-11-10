//! The built-in trigger inside the gateway daemon (spec: scheduled-jobs,
//! Built-in trigger inside the gateway daemon; Durable delivery outbox;
//! Pluggable trigger provider): one loop per served profile, under that
//! profile's own scope and store, holding the profile's tick lease while
//! it ticks every `cron.tick_interval_secs` (and at the earliest next
//! run), sending the profile's queued deliveries through the gateway.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use jiff::Timestamp;
use pmt_store::cron::{CronLease, TickLease};
use pmt_store::leases::Holder;
use tracing::Instrument as _;

use super::cron_send::GatewaySender;
use super::{DaemonHost, Profile};
use crate::cron_host::{self, HostRunner};

/// How often new served profiles are looked for.
const PROFILES_EVERY: Duration = Duration::from_secs(30);
/// How often retention is applied.
const PRUNE_EVERY: Duration = Duration::from_hours(1);

/// Starts a loop for every served profile (and for profiles a reload
/// serves later) until the daemon stops.
pub(in crate::daemon_cmd) async fn schedule(host: Arc<DaemonHost>) {
    let mut started = BTreeSet::new();
    pmt_obs::health::global().report(None, "scheduler", pmt_obs::health::State::Ok, "running");
    loop {
        for (name, profile) in host.all_profiles() {
            if started.insert(name.clone()) {
                let span =
                    tracing::info_span!("cron", component = "agent", profile = name.as_str());
                pmt_core::spawn::spawn(
                    profile_loop(Arc::clone(&host), name, profile).instrument(span),
                );
            }
        }
        tokio::time::sleep(PROFILES_EVERY).await;
    }
}

/// The tick lease of one profile.
struct Lease {
    held: Option<TickLease>,
    holder: Holder,
}

impl Lease {
    /// Holds (or renews) the lease; `false` while another process (a
    /// headless tick) has it. A first acquisition finalizes the runs a
    /// previous process left.
    fn hold(&mut self, store: &pmt_store::Store, ttl: Duration) -> bool {
        if let Some(lease) = &self.held
            && store.cron_renew_lease(lease).unwrap_or(false)
        {
            return true;
        }
        self.held = match store.cron_acquire_lease(&self.holder, "daemon", ttl) {
            Ok(CronLease::Granted(lease)) => Some(lease),
            Ok(CronLease::Busy(holder)) => {
                tracing::debug!(pid = holder.holder.pid, "cron tick lease held elsewhere");
                None
            }
            Err(err) => {
                tracing::warn!(error = %err, "cron tick lease");
                None
            }
        };
        if self.held.is_some()
            && let Err(err) = pmt_cron::scheduler::recover(store)
        {
            tracing::warn!(error = %err, "cron recovery failed");
        }
        self.held.is_some()
    }
}

async fn profile_loop(host: Arc<DaemonHost>, name: String, profile: Arc<Profile>) {
    let first = profile.scope();
    if let (_, Some(reason)) = pmt_cron::doctor::effective_provider(&first.config().cron) {
        tracing::warn!(profile = name.as_str(), "{reason}");
    }
    let permits = cron_host::permits(&first.config().cron);
    let sender = GatewaySender {
        host: Arc::clone(&host),
        profile: name.clone(),
        store: Arc::clone(&profile.store),
    };
    let mut lease = Lease {
        held: None,
        holder: Holder::current(),
    };
    let mut pruned = std::time::Instant::now().checked_sub(PRUNE_EVERY);
    loop {
        // Each tick reads the profile's live configuration.
        let scope = profile.scope();
        let config = scope.config().cron.clone();
        let interval = Duration::from_secs(config.tick_interval_secs.clamp(10, 300));
        if lease.hold(&profile.store, interval * 3) {
            let runner = Arc::new(HostRunner::new(
                &scope,
                &host.launch,
                Arc::clone(&profile.store),
            ));
            if pruned.is_none_or(|at| at.elapsed() >= PRUNE_EVERY) {
                let _ = pmt_cron::scheduler::prune(&profile.store, Timestamp::now(), &config);
                pruned = Some(std::time::Instant::now());
            }
            tick(&runner, &permits, &sender).await;
        }
        // A webhook's queued run wakes the loop early.
        tokio::select! {
            () = tokio::time::sleep(next_wake(&profile.store, interval)) => {}
            () = profile.cron_wake.notified() => {}
        }
    }
}

/// One tick of a profile holding its lease: due runs start on their own
/// tasks, then queued deliveries are sent.
async fn tick(
    runner: &Arc<HostRunner>,
    permits: &Arc<tokio::sync::Semaphore>,
    sender: &GatewaySender,
) {
    let now = Timestamp::now();
    match cron_host::dispatch(runner, now, permits) {
        Ok(running) if !running.is_empty() => {
            pmt_core::spawn::spawn(finish(running));
        }
        Ok(_) => {}
        Err(err) => tracing::warn!(error = %err, "cron tick failed"),
    }
    if let Err(err) = pmt_cron::deliver::pump(runner.store(), sender, now.as_millisecond()).await {
        tracing::warn!(error = %err, "cron delivery failed");
    }
}

/// Awaits the runs of one tick, logging what could not be recorded.
async fn finish(mut running: cron_host::Running) {
    while let Some(done) = running.join_next().await {
        match done {
            Ok(Ok(_)) => {}
            Ok(Err(err)) => tracing::warn!(error = %err, "cron run failed to record"),
            Err(err) => tracing::warn!(error = %err, "cron run panicked"),
        }
    }
}

/// Sleep until the earliest next run, at most `interval` and at least one
/// second (deliveries and queued runs are picked up each wake).
fn next_wake(store: &pmt_store::Store, interval: Duration) -> Duration {
    let now = Timestamp::now().as_millisecond();
    let earliest = store
        .cron_jobs()
        .unwrap_or_default()
        .iter()
        .filter(|j| j.enabled)
        .filter_map(|j| j.next_run_at)
        .min();
    let until = earliest.map_or(interval, |at| {
        Duration::from_millis(u64::try_from(at - now).unwrap_or(0))
    });
    until.clamp(Duration::from_secs(1), interval)
}

//! The daemon's evolution runner (spec: harness-evolution, Job lifecycle,
//! events and RPC; Resume and content-addressed session cache): every
//! few seconds each profile with evolution on has its interrupted jobs
//! queued again (once, at the daemon's start, when
//! `evolve.resume_on_start`) and its queued jobs started.

use std::sync::Arc;
use std::time::Duration;

use super::DaemonHost;

/// How often the runner looks for queued jobs.
const EVERY: Duration = Duration::from_secs(2);

/// Runs forever.
pub(in crate::daemon_cmd) async fn schedule(host: Arc<DaemonHost>) {
    let runner = crate::evolve_host::Runner::default();
    loop {
        for (_, profile) in host.all_profiles() {
            runner.tick(&profile.scope(), &host.launch);
        }
        tokio::time::sleep(EVERY).await;
    }
}

//! Local resource telemetry (spec: local-models, Local resource
//! telemetry): `session.context_usage` carries the GPU utilisation, GPU
//! memory and system RAM in use while the session's local model runs on
//! the managed server, for the TUI and desktop status bars. The machine is
//! sampled at most once every [`EVERY`] per home, however often the status
//! bars ask.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use pmt_agent::turn::model::RouteInfo;
use pmt_config::ProfileScope;
use pmt_rpc::contracts::LocalResources;

/// The shortest time between two samples.
pub(super) const EVERY: Duration = Duration::from_secs(2);

/// The last sample of each home.
static LAST: Mutex<Vec<(PathBuf, Instant, LocalResources)>> = Mutex::new(Vec::new());

/// The local runtime's use now (or as sampled within [`EVERY`]), when
/// `route` is a local model on the home's managed server.
pub(super) fn sample(scope: &ProfileScope, route: Option<&RouteInfo>) -> Option<LocalResources> {
    route.filter(|r| r.local && r.provider == "llamacpp")?;
    let home = scope.home().to_path_buf();
    pmt_local::server::record::live(&home)?;
    let mut last = LAST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((_, at, resources)) = last.iter().find(|(h, _, _)| *h == home)
        && at.elapsed() < EVERY
    {
        return Some(resources.clone());
    }
    let local =
        crate::local_cmd::Local::for_home(&home, pmt_net::OutboundPolicy::default()).ok()?;
    let hardware = local.hardware();
    let gpu = hardware.gpus.first();
    let resources = LocalResources {
        gpu_utilization_percent: gpu.and_then(|g| g.utilization_percent),
        gpu_memory_used: gpu.map_or(0, |g| g.memory_used),
        gpu_memory_total: gpu.map_or(0, |g| g.memory_total),
        ram_used: hardware.ram_total.saturating_sub(hardware.ram_available),
        ram_total: hardware.ram_total,
    };
    last.retain(|(h, _, _)| *h != home);
    last.push((home, Instant::now(), resources.clone()));
    Some(resources)
}

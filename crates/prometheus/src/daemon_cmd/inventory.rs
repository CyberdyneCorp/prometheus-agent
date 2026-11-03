//! `prometheus gateway list` and the daemon's entry in the machine-global
//! registry (spec: messaging-gateway, Daemon inventory across homes): each
//! running daemon records its home, pid and start time, build, served
//! profiles and enabled adapters in the OS user's runtime directory; the
//! list reads every home's entry, checks each pid against its start time
//! (never argv) and prunes the stale ones.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use pmt_core::env::LaunchEnv;
use pmt_core::error::Error;
use pmt_core::inventory::{self, DaemonEntry};
use serde::Serialize;

use crate::context::Context;
use crate::output::{document, print_raw};

/// Records this daemon of `home` serving `profiles` (their enabled
/// adapters by profile). A home without a user runtime directory is only
/// logged: the daemon runs the same, it is just not listed.
pub(super) fn register(home: &Path, launch: &LaunchEnv, adapters: BTreeMap<String, Vec<String>>) {
    let Some(dir) = inventory::registry_dir(launch) else {
        tracing::warn!("no user home: this daemon is not listed by 'gateway list'");
        return;
    };
    let mut entry = DaemonEntry::current(
        home,
        crate::build_info::BUILD_ID,
        adapters.keys().cloned().collect(),
    );
    entry.adapters = adapters;
    if let Err(err) = inventory::register(&dir, &entry) {
        tracing::warn!("this daemon is not listed by 'gateway list': {err}");
    }
}

/// Removes this daemon's entry.
pub(super) fn unregister(home: &Path, launch: &LaunchEnv) {
    if let Some(dir) = inventory::registry_dir(launch) {
        inventory::unregister(&dir, home);
    }
}

/// One daemon in `gateway list --json`.
#[derive(Debug, Serialize)]
struct Daemon {
    home: String,
    pid: u32,
    started_at: String,
    version: String,
    build_id: String,
    profiles: Vec<String>,
    adapters: BTreeMap<String, Vec<String>>,
    /// `running`, or `stale` (its process is gone; the entry was pruned).
    state: &'static str,
}

fn started_at(secs: u64) -> String {
    i64::try_from(secs)
        .ok()
        .and_then(|s| jiff::Timestamp::from_second(s).ok())
        .map_or_else(String::new, |t| t.to_string())
}

/// `prometheus gateway list [--json]`.
///
/// # Errors
///
/// `home.io` when the registry cannot be read.
pub(super) fn list(ctx: &Context<'_>) -> Result<u8, Error> {
    let listed = match inventory::registry_dir(ctx.launch) {
        Some(dir) => inventory::list(&dir)?,
        None => Vec::new(),
    };
    let daemons: Vec<Daemon> = listed
        .into_iter()
        .map(|l| Daemon {
            home: l.entry.home.to_string_lossy().into_owned(),
            pid: l.entry.pid,
            started_at: started_at(l.entry.start_time),
            version: l.entry.version,
            build_id: l.entry.build_id,
            profiles: l.entry.profiles,
            adapters: l.entry.adapters,
            state: if l.alive { "running" } else { "stale" },
        })
        .collect();
    if ctx.json {
        return document("gateway list", &serde_json::json!({"daemons": daemons}));
    }
    if daemons.is_empty() {
        return print_raw("no Prometheus daemon is running\n");
    }
    print_raw(&render(&daemons))
}

/// The plain listing: one line per daemon, one per served profile.
fn render(daemons: &[Daemon]) -> String {
    let mut text = String::new();
    for daemon in daemons {
        let _ = writeln!(
            text,
            "{} (pid {}, started {}, build {}){}",
            daemon.home,
            daemon.pid,
            daemon.started_at,
            daemon.build_id,
            if daemon.state == "stale" {
                " stale, pruned"
            } else {
                ""
            }
        );
        for profile in &daemon.profiles {
            let adapters = daemon.adapters.get(profile).cloned().unwrap_or_default();
            let adapters = if adapters.is_empty() {
                "no messaging adapters".to_owned()
            } else {
                adapters.join(", ")
            };
            let _ = writeln!(text, "  {profile}: {adapters}");
        }
    }
    text
}

//! The `NeMo` Relay host (spec: observability, `NeMo` Relay and Langfuse
//! integrations). With `observability.nemo_relay.enabled` the process
//! (daemon, chat or cron run; the launch profile's setting applies to
//! every profile it serves) runs `relay_host.py` of the python bridge in a
//! managed environment holding [`REQUIREMENT`], and every session's
//! lifecycle reaches it through [`pmt_obs::relay::feed`]. Relay loads its
//! user file under the launch user's home, replaced by
//! `observability.nemo_relay.plugins_toml` when set, then the system file,
//! which has the highest precedence.
//!
//! The environment is built in the background on first use (network for
//! the lock, then offline-capable): sessions never wait for it, and a host
//! that cannot start is logged and drops what it was fed.

mod process;

use std::path::{Path, PathBuf};

use pmt_config::ProfileScope;
use pmt_core::env::LaunchEnv;
use pmt_obs::relay::{RelayFile, files as relay_files, system_file};

pub use process::{REQUIREMENT, start, stop};

/// The Relay files that `scope` would load, lowest precedence first.
#[must_use]
pub fn files(scope: &ProfileScope, launch: &LaunchEnv) -> Vec<RelayFile> {
    let user_home = launch.user_home().cloned().unwrap_or_default();
    relay_files(
        &user_home,
        configured(scope, launch).as_deref(),
        &system(launch),
    )
}

/// `observability.nemo_relay.plugins_toml`, `~` expanded.
fn configured(scope: &ProfileScope, launch: &LaunchEnv) -> Option<PathBuf> {
    let user_home = launch.user_home().cloned().unwrap_or_default();
    scope
        .config()
        .observability
        .nemo_relay
        .plugins_toml
        .as_deref()
        .map(|p| expand(p, &user_home))
}

fn expand(path: &str, user_home: &Path) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => user_home.join(rest),
        None => PathBuf::from(path),
    }
}

/// The system file (`%ProgramData%\nemo-relay\plugins.toml` on Windows);
/// debug builds' tests point it at a directory of their own.
fn system(launch: &LaunchEnv) -> PathBuf {
    #[cfg(debug_assertions)]
    if let Some(dir) = std::env::var_os("PMT_TEST_RELAY_SYSTEM_DIR") {
        return PathBuf::from(dir).join("plugins.toml");
    }
    system_file(launch.inherited("ProgramData").map(Path::new))
}

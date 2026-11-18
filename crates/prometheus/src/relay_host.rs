//! The `NeMo` Relay files of a profile (spec: observability, `NeMo` Relay and
//! Langfuse integrations): Relay's user file under the launch user's
//! home, replaced by `observability.nemo_relay.plugins_toml` when set,
//! then the system file, which has the highest precedence.

use std::path::{Path, PathBuf};

use pmt_config::ProfileScope;
use pmt_core::env::LaunchEnv;
use pmt_obs::relay::{RelayFile, files as relay_files, system_file};

/// The Relay files that `scope` would load, lowest precedence first.
#[must_use]
pub fn files(scope: &ProfileScope, launch: &LaunchEnv) -> Vec<RelayFile> {
    let user_home = launch.user_home().cloned().unwrap_or_default();
    let configured = scope
        .config()
        .observability
        .nemo_relay
        .plugins_toml
        .clone()
        .map(|p| expand(&p, &user_home));
    relay_files(&user_home, configured.as_deref(), &system(launch))
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

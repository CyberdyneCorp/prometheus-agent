//! What every `prometheus` process does before running its command: read
//! the launch environment once, resolve the home, raise the open-file
//! limit and set up crash reports.
//!
//! None of these steps may stop a command that does not need the home
//! (`version`, `help`): a home that cannot be resolved or an invalid
//! `daemon.toml` is reported and the defaults are used.

use std::path::{Path, PathBuf};

use pmt_config::daemon::{DaemonConfig, ProcessConfig};
use pmt_core::env::{EnvPolicy, LaunchEnv};
use pmt_core::error::Error;
use pmt_core::nofile::{self, NofileLimit};
use pmt_core::panic::CrashReporter;
use pmt_core::secret::Redactor;

/// Reads the process environment, once.
///
/// # Errors
///
/// `launch_env.reread` if called twice.
pub fn launch_env() -> Result<LaunchEnv, Error> {
    // Provider-native secret names join the policy with the provider
    // registry; until then only `PMT_SECRET_*` names are secrets.
    LaunchEnv::from_process(&EnvPolicy::default())
}

/// Logs one WARN per ignored environment variable.
pub fn warn_ignored_env(launch: &LaunchEnv) {
    for var in launch.ignored() {
        tracing::warn!("{}", var.warning());
    }
}

/// Raises the open-file soft limit to `process.nofile_soft_limit` from the
/// home's `daemon.toml` (the default when there is no home or file), and
/// logs a WARN when the result is below that target.
pub fn raise_nofile(home: Option<&Path>) -> Option<NofileLimit> {
    let target = match home.map(DaemonConfig::load) {
        Some(Ok(config)) => config.process.nofile_soft_limit,
        Some(Err(err)) => {
            tracing::warn!("{err}; using the default process.nofile_soft_limit");
            ProcessConfig::default().nofile_soft_limit
        }
        None => ProcessConfig::default().nofile_soft_limit,
    };
    match nofile::raise(target) {
        Ok(Some(limit)) => {
            if limit.below_target() {
                tracing::warn!("{}", limit.warning());
            }
            Some(limit)
        }
        Ok(None) => None,
        Err(err) => {
            tracing::warn!("cannot read the open-file limit: {err}");
            None
        }
    }
}

/// Crash reports go to `<home>/logs/crash/`, redacting every secret from
/// the launch environment; without a home, to the system temp directory.
#[must_use]
pub fn crash_reporter(home: Option<&PathBuf>, launch: &LaunchEnv) -> CrashReporter {
    let redactor = Redactor::new(launch.secrets().map(|(_, value)| value));
    match home {
        Some(home) => CrashReporter::for_home(home, redactor),
        None => CrashReporter::new(std::env::temp_dir().join("prometheus-crash"), redactor),
    }
}

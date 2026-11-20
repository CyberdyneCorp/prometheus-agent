//! Backend selection and availability (spec: computer-use): `cua` (the
//! pinned `cua-driver` this build launches itself) or the backend an
//! enabled `computer-use-backend` plugin provides under its name.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use pmt_config::daemon::DaemonConfig;
use pmt_config::schema::{CUA_BACKEND, ComputerUseConfig};
use pmt_config::{Config, Home, Launch, Overrides, ProfileName, ProfileScope};
use pmt_core::error::{Error, codes};
use pmt_plugins::computer_use::PluginDesktop;
use pmt_plugins::host::CallInfo;
use pmt_tools::probe::{Probe, ProbeContext, ProbeFuture};

use super::{binary, platform};
use crate::plugins_host::ProfilePlugins;

/// The id of the tool's availability probe.
pub const PROBE_ID: &str = "computer_use.backend";

/// How stale the plugin set a probe reads may be.
const PLUGIN_RECHECK: Duration = Duration::from_secs(30);

/// The backends a profile may name: `cua`, plus those plugins provide.
#[must_use]
pub fn backends(plugin_backends: &[PluginDesktop]) -> Vec<String> {
    std::iter::once(CUA_BACKEND.to_owned())
        .chain(plugin_backends.iter().map(|b| b.name.clone()))
        .collect()
}

/// The computer-use backends `plugins` provide now, read as `call`.
#[must_use]
pub fn plugin_backends(plugins: &ProfilePlugins, call: &CallInfo) -> Vec<PluginDesktop> {
    pmt_plugins::computer_use::backends(&plugins.current(), call)
}

/// The plugin backend `name` among those `plugins` provide.
#[must_use]
pub fn plugin_backend(
    name: &str,
    plugins: &ProfilePlugins,
    call: &CallInfo,
) -> Option<PluginDesktop> {
    plugin_backends(plugins, call)
        .into_iter()
        .find(|b| b.name == name)
}

/// Checks `computer_use.backend` against `cua` and the plugin backends.
///
/// # Errors
///
/// `config.invalid` (78) naming the unknown backend.
pub fn check_backend(
    config: &ComputerUseConfig,
    plugin_backends: &[PluginDesktop],
) -> Result<(), Error> {
    let known = backends(plugin_backends);
    if known.contains(&config.backend) {
        return Ok(());
    }
    Err(Error::new(
        &codes::CONFIG_INVALID,
        format!(
            "computer_use.backend: unknown computer-use backend '{}' (expected {})",
            config.backend,
            known.join(", ")
        ),
    ))
}

/// The tool's availability for the home at `home`: the backend is ready
/// (the driver is installed or can be installed lazily, or the plugin
/// backend's `check` passes) and this platform's prerequisites pass. Read
/// when the probe runs, off the async threads, so installs, plugin changes
/// and `daemon.toml` edits apply at the next surface build.
#[must_use]
pub fn probe(home: PathBuf) -> Arc<dyn Probe> {
    Arc::new(BackendProbe { home })
}

struct BackendProbe {
    home: PathBuf,
}

impl Probe for BackendProbe {
    fn id(&self) -> &str {
        PROBE_ID
    }

    fn check<'a>(&'a self, context: ProbeContext<'a>) -> ProbeFuture<'a> {
        let home = self.home.clone();
        let profile = context.profile.to_owned();
        let config = context.config.clone();
        Box::pin(async move {
            pmt_core::spawn::blocking(move || {
                if config.computer_use.backend == CUA_BACKEND {
                    cua_ready(&home, &config)
                } else {
                    plugin_ready(&home, &profile, &config.computer_use.backend)
                }
            })
            .await
            .map_err(|e| e.to_string())?
        })
    }
}

/// The plugin backend `name` of profile `profile` answers its readiness
/// check.
fn plugin_ready(home: &Path, profile: &str, name: &str) -> Result<(), String> {
    let scope = Home::open(home)
        .and_then(|home| home.existing_profile(&ProfileName::parse(profile)?))
        .and_then(|dir| ProfileScope::open(dir, &Overrides::default(), Launch::Other))
        .map_err(|e| e.message().to_owned())?;
    let plugins = crate::plugins_host::current(&scope, PLUGIN_RECHECK);
    plugin_backend(name, &plugins, &CallInfo::default())
        .ok_or_else(|| not_provided(name))?
        .check()
        .map(drop)
}

/// Why a named backend is unavailable when no enabled plugin provides it.
#[must_use]
pub fn not_provided(name: &str) -> String {
    format!("computer-use backend '{name}' is not installed or not enabled")
}

/// `cua`: the driver is installed or installable and the platform's
/// prerequisites pass.
fn cua_ready(home: &Path, config: &Config) -> Result<(), String> {
    let place = crate::screen::placement::resolve(&config.bot_desktop, &config.terminal.backend)
        .map_err(|e| e.message().to_owned())?;
    if place != crate::screen::placement::Where::Host {
        // The desktop image carries the driver and the screen.
        return Ok(());
    }
    let computer_use = &config.computer_use;
    let daemon = DaemonConfig::load(home).map_err(|e| e.message().to_owned())?;
    binary::installed(home, computer_use, &daemon)?;
    let vars = |name: &str| std::env::var_os(name);
    let mut facts = platform::Facts::of(&vars);
    if config.bot_desktop.auto_start {
        // The bot screen starts with the first action and brings its
        // display and session bus.
        facts.display = true;
        facts.session_bus = true;
    }
    // macOS grants are the driver's to report; they are checked when a
    // session drives (and by `computer-use permissions status`).
    let grants = platform::grants(computer_use, &facts, None);
    let checked: Vec<_> = grants
        .into_iter()
        .filter(|g| facts.os != "macos" || g.granted)
        .collect();
    platform::missing(&checked).map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use pmt_config::schema::ComputerUseConfig;

    use super::check_backend;

    #[test]
    fn an_unknown_backend_fails_validation() {
        let config = ComputerUseConfig {
            backend: "nope".into(),
            ..ComputerUseConfig::default()
        };
        let err = check_backend(&config, &[]).unwrap_err();
        assert_eq!(err.exit_code(), 78);
        assert!(
            err.message()
                .contains("unknown computer-use backend 'nope' (expected cua)")
        );
        assert!(check_backend(&ComputerUseConfig::default(), &[]).is_ok());
    }
}

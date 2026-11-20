//! The host side of computer use (spec: computer-use): which driver runs
//! ([`binary`]), how its runtimes start and where ([`launcher`]), its
//! macOS identity ([`identity`]), the platform prerequisites and cursor
//! overlay ([`platform`]) and the health report ([`doctor`]). The tool
//! itself is `pmt_tools::computer_use`.

pub mod binary;
pub mod doctor;
pub mod identity;
pub mod launcher;
pub mod platform;

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;

use pmt_config::ProfileScope;
use pmt_config::daemon::DaemonConfig;
use pmt_config::schema::{CUA_BACKEND, ComputerUseConfig, ComputerUsePermission};
use pmt_core::env::LaunchEnv;
use pmt_core::error::{Error, codes};
use pmt_tools::computer_use::{ComputerUseSession, SessionSetup};
use pmt_tools::probe::{FnProbe, Probe};

/// The id of the tool's availability probe.
pub const PROBE_ID: &str = "computer_use.backend";

/// The backends a profile may name: `cua`, plus those plugins register.
#[must_use]
pub fn backends(plugin_backends: &[&str]) -> Vec<String> {
    std::iter::once(CUA_BACKEND)
        .chain(plugin_backends.iter().copied())
        .map(str::to_owned)
        .collect()
}

/// Checks `computer_use.backend`.
///
/// # Errors
///
/// `config.invalid` (78) naming the unknown backend.
pub fn check_backend(config: &ComputerUseConfig, plugin_backends: &[&str]) -> Result<(), Error> {
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

/// A bounded profile's manifest must be readable when a session that can
/// use computer use starts: it never runs unbounded.
///
/// # Errors
///
/// `computer_use.manifest` (78) with the path.
pub fn check_session_start(config: &ComputerUseConfig) -> Result<(), Error> {
    if config.permission_mode != ComputerUsePermission::Bounded {
        return Ok(());
    }
    launcher::check_manifest(config.capability_manifest.as_deref())
        .map_err(|why| Error::new(&codes::COMPUTER_USE_MANIFEST, why))
}

/// The launch variables of `launch`.
fn launch_vars(launch: &LaunchEnv) -> impl Fn(&str) -> Option<OsString> + '_ {
    move |name: &str| launch.inherited(name).map(OsString::from)
}

/// The computer use of one session of `scope` (`None`: the backend is
/// not `cua`, which this build launches itself).
#[must_use]
pub fn session_part(
    scope: &ProfileScope,
    session_id: &str,
    image_input: bool,
    launch: &LaunchEnv,
) -> Option<Arc<ComputerUseSession>> {
    let config = scope.config().computer_use.clone();
    if config.backend != CUA_BACKEND {
        return None;
    }
    let vars = launch_vars(launch);
    let facts = platform::Facts::of(&vars);
    let env = launcher::driver_env(scope, &vars);
    let overlay = platform::overlay(&config, &facts);
    Some(Arc::new(ComputerUseSession::new(SessionSetup {
        session_id: session_id.to_owned(),
        profile_dir: scope.dir().to_path_buf(),
        config,
        image_input,
        overlay,
        launcher: Arc::new(launcher::CuaLauncher::new(scope.clone(), env)),
    })))
}

/// The tool's availability for the home at `home`: the backend is ready
/// (the driver is installed or can be installed lazily) and this
/// platform's prerequisites pass. Read when the probe runs, so installs
/// and `daemon.toml` edits apply at the next surface build.
#[must_use]
pub fn probe(home: PathBuf) -> Arc<dyn Probe> {
    Arc::new(FnProbe::new(PROBE_ID, move |context| {
        let config = &context.config.computer_use;
        if config.backend != CUA_BACKEND {
            return Err(format!(
                "computer-use backend '{}' is not installed or not enabled",
                config.backend
            ));
        }
        let place = crate::screen::placement::resolve(
            &context.config.bot_desktop,
            &context.config.terminal.backend,
        )
        .map_err(|e| e.message().to_owned())?;
        if place != crate::screen::placement::Where::Host {
            // The desktop image carries the driver and the screen.
            return Ok(());
        }
        let daemon = DaemonConfig::load(&home).map_err(|e| e.message().to_owned())?;
        binary::installed(&home, config, &daemon)?;
        let vars = |name: &str| std::env::var_os(name);
        let mut facts = platform::Facts::of(&vars);
        if context.config.bot_desktop.auto_start {
            // The bot screen starts with the first action and brings its
            // display and session bus.
            facts.display = true;
            facts.session_bus = true;
        }
        // macOS grants are the driver's to report; they are checked when
        // a session drives (and by `computer-use permissions status`).
        let grants = platform::grants(config, &facts, None);
        let checked: Vec<_> = grants
            .into_iter()
            .filter(|g| facts.os != "macos" || g.granted)
            .collect();
        platform::missing(&checked).map_or(Ok(()), Err)
    }))
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
        assert!(
            check_backend(&config, &["nope"]).is_ok(),
            "a plugin's backend is known"
        );
        assert!(check_backend(&ComputerUseConfig::default(), &[]).is_ok());
    }
}

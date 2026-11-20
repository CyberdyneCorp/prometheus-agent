//! The host side of computer use (spec: computer-use): which backend
//! drives the desktop and whether it is ready ([`backend`]), which driver
//! runs ([`binary`]), how its runtimes start and where ([`launcher`]), its
//! macOS identity ([`identity`]), the platform prerequisites and cursor
//! overlay ([`platform`]) and the health report ([`doctor`]). The tool
//! itself is `pmt_tools::computer_use`.

pub mod backend;
pub mod binary;
pub mod doctor;
pub mod identity;
pub mod launcher;
pub mod platform;

use std::ffi::OsString;
use std::sync::Arc;

use pmt_config::ProfileScope;
use pmt_config::schema::{CUA_BACKEND, ComputerUseConfig, ComputerUsePermission, Overlay};
use pmt_core::env::LaunchEnv;
use pmt_core::error::{Error, codes};
use pmt_plugins::computer_use::PluginDesktop;
use pmt_tools::computer_use::{ComputerUseSession, Launcher, SessionSetup};

pub use backend::{PROBE_ID, check_backend, probe};

/// A bounded profile's manifest must be readable when any of its sessions
/// starts, whatever toolsets it selects (`/tools` can enable
/// `computer_use` later in the session): it never runs unbounded.
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

/// The computer use of one session of `scope`: `cua` launches the pinned
/// driver itself, any other backend is `plugin`'s (`None` when no enabled
/// plugin provides it: the tool is unavailable).
#[must_use]
pub fn session_part(
    scope: &ProfileScope,
    session_id: &str,
    image_input: bool,
    launch: &LaunchEnv,
    plugin: Option<PluginDesktop>,
) -> Option<Arc<ComputerUseSession>> {
    let config = scope.config().computer_use.clone();
    let (launcher, overlay): (Arc<dyn Launcher>, bool) = if config.backend == CUA_BACKEND {
        let vars = launch_vars(launch);
        let facts = platform::Facts::of(&vars);
        let env = launcher::driver_env(scope, &vars);
        let overlay = platform::overlay(&config, &facts);
        (
            Arc::new(launcher::CuaLauncher::new(scope.clone(), env)),
            overlay,
        )
    } else {
        // The plugin's desktop decides how it shows the cursor.
        (plugin?.launcher(), config.overlay != Overlay::Off)
    };
    Some(Arc::new(ComputerUseSession::new(SessionSetup {
        session_id: session_id.to_owned(),
        profile_dir: scope.dir().to_path_buf(),
        config,
        image_input,
        overlay,
        launcher,
    })))
}

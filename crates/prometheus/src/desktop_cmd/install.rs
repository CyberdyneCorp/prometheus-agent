//! `prometheus desktop --install` (spec: desktop-app, Launching the
//! desktop from the CLI): the desktop package of this binary's version
//! and channel, verified against the embedded release keys before
//! anything is written. A download whose signature does not verify is
//! dropped (it never reaches the disk) and the command exits 77.

use std::path::Path;

use pmt_config::daemon::DaemonConfig;
use pmt_core::error::{Error, codes};
use pmt_update::{Identity, Releases, TrustRoot};

use super::locate;
use crate::build_info;
use crate::context::Context;
use crate::output::print_raw;

/// The desktop package published for `target` (`cargo xtask release`
/// names them so).
#[must_use]
pub(crate) fn package_name(target: &str) -> Option<String> {
    let arch = target.split('-').next()?;
    if target.ends_with("apple-darwin") {
        Some("prometheus-desktop-universal2-apple-darwin.app.tar.gz".to_owned())
    } else if target.contains("-linux-") && !target.contains("android") {
        Some(format!("prometheus-desktop-{arch}-linux.AppImage"))
    } else if target.ends_with("windows-msvc") {
        // One bundle holds the x86_64 and aarch64 packages.
        Some("prometheus-desktop-x86_64-pc-windows-msvc.msixbundle".to_owned())
    } else {
        None
    }
}

fn failed(message: String) -> Error {
    Error::new(&codes::UPDATE_FAILED, message)
}

/// Downloads, verifies and installs the desktop package.
pub(super) fn install(ctx: &Context<'_>, user_home: &Path) -> Result<u8, Error> {
    let name = package_name(build_info::TARGET).ok_or_else(|| {
        failed(format!(
            "no desktop package is published for {}",
            build_info::TARGET
        ))
    })?;
    let home = ctx.home()?;
    let config = DaemonConfig::load(home.root())?.update;
    let releases = Releases::new(
        config.mirror_url.as_deref(),
        Identity {
            version: build_info::VERSION,
            target: build_info::TARGET,
        },
        &crate::update_cmd::outbound_policy(ctx),
        TrustRoot::embedded(),
        config.require_transparency_log,
    )?
    .with_fallback(config.mirror_fallback);
    let path = format!("v{}/{name}", build_info::VERSION);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| failed(format!("cannot start the runtime: {e}")))?;
    eprintln!(
        "downloading {name} ({} channel)",
        build_info::channel(build_info::VERSION)
    );
    let bytes = runtime.block_on(releases.verified(&path))?;
    let target = locate::install_target(user_home, ctx.launch);
    place(&bytes, &name, &target, home.root())?;
    print_raw(&format!(
        "installed Prometheus Desktop {} at {}\n",
        build_info::VERSION,
        target.display()
    ))
}

/// Writes the verified package where it is installed from.
fn place(bytes: &[u8], name: &str, target: &Path, home: &Path) -> Result<(), Error> {
    let staging = home.join("updates/desktop");
    std::fs::create_dir_all(&staging)
        .map_err(|e| Error::io(&codes::UPDATE_FAILED, &staging, &e))?;
    let file = staging.join(name);
    std::fs::write(&file, bytes).map_err(|e| Error::io(&codes::UPDATE_FAILED, &file, &e))?;
    let installed = install_package(&file, target);
    let _ = std::fs::remove_file(&file);
    installed
}

#[cfg(not(windows))]
fn install_package(file: &Path, target: &Path) -> Result<(), Error> {
    use pmt_desktop::update_install::{Target, install_archive};
    let target = if target.extension().is_some_and(|e| e == "app") {
        Target::AppBundle(target.to_owned())
    } else {
        Target::AppImage(target.to_owned())
    };
    install_archive(file, &target)
        .map_err(|e| failed(format!("could not install the desktop app: {e}")))
}

/// MSIX packages install through App Installer's deployment API.
#[cfg(windows)]
fn install_package(file: &Path, _target: &Path) -> Result<(), Error> {
    let status = std::process::Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Add-AppxPackage",
            "-Path",
        ])
        .arg(file)
        .status()
        .map_err(|e| failed(format!("could not start Add-AppxPackage: {e}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(failed(format!("Add-AppxPackage failed ({status})")))
    }
}

#[cfg(test)]
mod tests {
    use super::package_name;

    #[test]
    fn the_app_updater_and_the_cli_use_the_same_release_origin() {
        assert_eq!(
            pmt_desktop::update::DEFAULT_ORIGIN,
            pmt_update::DEFAULT_ORIGIN
        );
    }

    #[test]
    fn every_desktop_target_has_its_package() {
        assert_eq!(
            package_name("aarch64-apple-darwin").as_deref(),
            Some("prometheus-desktop-universal2-apple-darwin.app.tar.gz")
        );
        assert_eq!(
            package_name("x86_64-unknown-linux-musl").as_deref(),
            Some("prometheus-desktop-x86_64-linux.AppImage")
        );
        assert_eq!(
            package_name("aarch64-pc-windows-msvc").as_deref(),
            Some("prometheus-desktop-x86_64-pc-windows-msvc.msixbundle")
        );
        assert_eq!(package_name("aarch64-linux-android"), None);
    }
}

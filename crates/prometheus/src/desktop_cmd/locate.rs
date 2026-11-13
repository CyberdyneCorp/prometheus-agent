//! Where the desktop app is installed (spec: desktop-app, Launching the
//! desktop from the CLI; distribution-and-update, Desktop packages and
//! signing): the executable to start, looked for where `--install` puts
//! it and where the packages install it.

use std::path::{Path, PathBuf};

use pmt_core::env::LaunchEnv;

/// The macOS app bundle's name.
pub(super) const MAC_BUNDLE: &str = "Prometheus.app";
/// The executable inside the bundle, the deb/rpm and MSIX packages.
pub(super) const EXECUTABLE: &str = "prometheus-desktop";
/// The Linux `AppImage` that `--install` writes.
pub(super) const APPIMAGE: &str = "Prometheus.AppImage";

/// Where `--install` installs the app on this OS.
pub(super) fn install_target(user_home: &Path, launch: &LaunchEnv) -> PathBuf {
    if cfg!(target_os = "macos") {
        user_home.join("Applications").join(MAC_BUNDLE)
    } else if cfg!(windows) {
        launch
            .local_app_data()
            .cloned()
            .unwrap_or_else(|| user_home.join("AppData/Local"))
            .join("Programs/Prometheus")
    } else {
        data_home(user_home, launch)
            .join("prometheus-desktop")
            .join(APPIMAGE)
    }
}

fn data_home(user_home: &Path, launch: &LaunchEnv) -> PathBuf {
    launch
        .inherited("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map_or_else(|| user_home.join(".local/share"), PathBuf::from)
}

/// The executable that starts an install at `target` (a bundle or a
/// directory or the `AppImage` itself).
fn executable(target: &Path) -> PathBuf {
    if target.extension().is_some_and(|e| e == "app") {
        target.join("Contents/MacOS").join(EXECUTABLE)
    } else if target.is_dir() {
        target.join(format!("{EXECUTABLE}{}", std::env::consts::EXE_SUFFIX))
    } else {
        target.to_path_buf()
    }
}

/// The installed app's executable, if any.
pub(super) fn installed(user_home: &Path, launch: &LaunchEnv) -> Option<PathBuf> {
    let mut candidates = vec![executable(&install_target(user_home, launch))];
    if cfg!(target_os = "macos") {
        candidates.push(executable(&Path::new("/Applications").join(MAC_BUNDLE)));
    } else if cfg!(windows) {
        // The MSIX package's execution alias.
        if let Some(local) = launch.local_app_data() {
            candidates.push(
                local
                    .join("Microsoft/WindowsApps")
                    .join(format!("{EXECUTABLE}.exe")),
            );
        }
    } else {
        candidates.push(Path::new("/usr/bin").join(EXECUTABLE));
    }
    candidates.into_iter().find(|p| p.is_file())
}

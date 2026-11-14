//! Installing a staged update when the app quits or restarts (spec:
//! desktop-app, Signed in-app updates). Only the installs the desktop
//! updater owns are replaced here: the macOS app bundle (the `.dmg`
//! install) and the Linux `.AppImage`. MSIX, Store, deb and rpm installs
//! are updated by App Installer, the Store or the system package manager,
//! so the app builds no updater for them ([`Target::detect`]).
//!
//! The archive was verified before it was staged; the swap renames a
//! complete new copy into place, so an interrupted install leaves the old
//! app intact.

use std::path::{Path, PathBuf};

use crate::update::{Staged, clear_staged};

/// What the running app was installed as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A macOS `.app` bundle (its path); updates are `.app.tar.gz`.
    AppBundle(PathBuf),
    /// A Linux `AppImage` file (its path); updates are the new `AppImage`.
    AppImage(PathBuf),
}

impl Target {
    /// The install of the executable `exe`, given the `APPIMAGE` variable
    /// the `AppImage` runtime sets; `None` for installs another updater owns.
    #[must_use]
    pub fn detect(exe: &Path, appimage: Option<&Path>) -> Option<Self> {
        if let Some(image) = appimage {
            return Some(Self::AppImage(image.to_owned()));
        }
        // <bundle>.app/Contents/MacOS/<exe>
        let bundle = exe.parent()?.parent()?.parent()?;
        let is_bundle = bundle.extension().is_some_and(|e| e == "app")
            && exe.parent()?.file_name().is_some_and(|n| n == "MacOS");
        is_bundle.then(|| Self::AppBundle(bundle.to_owned()))
    }

    /// The feed's platform key of this install on this machine.
    #[must_use]
    pub fn platform(&self) -> String {
        match self {
            Self::AppBundle(_) => "darwin-universal".to_owned(),
            Self::AppImage(_) => format!("linux-{}", std::env::consts::ARCH),
        }
    }
}

/// Replaces the app with the staged archive and forgets it.
///
/// # Errors
///
/// When the archive cannot be unpacked or the swap fails (the old app is
/// then still in place).
pub fn install(staged: &Staged, target: &Target, state_dir: &Path) -> std::io::Result<()> {
    install_archive(&staged.archive, target)?;
    clear_staged(state_dir);
    Ok(())
}

/// Installs the verified `archive` at `target`, replacing what is there
/// (`prometheus desktop --install` uses it for a first install too).
///
/// # Errors
///
/// When the archive cannot be unpacked or the swap fails (an existing app
/// is then still in place).
pub fn install_archive(archive: &Path, target: &Target) -> std::io::Result<()> {
    match target {
        Target::AppBundle(bundle) => install_bundle(archive, bundle),
        Target::AppImage(image) => install_image(archive, image),
    }
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!(".{name}.{suffix}"))
}

fn install_bundle(archive: &Path, bundle: &Path) -> std::io::Result<()> {
    if let Some(parent) = bundle.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let unpacked = sibling(bundle, "new");
    let _ = std::fs::remove_dir_all(&unpacked);
    std::fs::create_dir_all(&unpacked)?;
    let file = std::fs::File::open(archive)?;
    tar::Archive::new(flate2::read::GzDecoder::new(file)).unpack(&unpacked)?;
    // The archive holds `<Name>.app/`.
    let new_app = std::fs::read_dir(&unpacked)?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e == "app"))
        .ok_or_else(|| std::io::Error::other("the update archive holds no .app bundle"))?;
    if !bundle.exists() {
        std::fs::rename(&new_app, bundle)?;
        let _ = std::fs::remove_dir_all(&unpacked);
        return Ok(());
    }
    let old = sibling(bundle, "old");
    let _ = std::fs::remove_dir_all(&old);
    std::fs::rename(bundle, &old)?;
    if let Err(e) = std::fs::rename(&new_app, bundle) {
        std::fs::rename(&old, bundle)?;
        return Err(e);
    }
    let _ = std::fs::remove_dir_all(&old);
    let _ = std::fs::remove_dir_all(&unpacked);
    Ok(())
}

fn install_image(archive: &Path, image: &Path) -> std::io::Result<()> {
    if let Some(parent) = image.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let next = sibling(image, "new");
    std::fs::copy(archive, &next)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&next, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(&next, image)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::Target;

    #[test]
    fn only_bundles_and_appimages_are_updated_here() {
        let exe = Path::new("/Applications/Prometheus.app/Contents/MacOS/prometheus-desktop");
        assert_eq!(
            Target::detect(exe, None),
            Some(Target::AppBundle("/Applications/Prometheus.app".into()))
        );
        assert_eq!(
            Target::detect(Path::new("/usr/bin/prometheus-desktop"), None),
            None,
            "a deb/rpm install is the package manager's"
        );
        let image = Path::new("/home/u/Prometheus.AppImage");
        assert_eq!(
            Target::detect(Path::new("/tmp/.mount_x/usr/bin/app"), Some(image)),
            Some(Target::AppImage(image.into()))
        );
    }
}

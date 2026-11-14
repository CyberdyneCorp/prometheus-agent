//! The "Install command-line tool" action (spec: distribution-and-update,
//! Desktop packages and signing). The macOS app and the Linux `AppImage`
//! carry the release's `prometheus` binary but put nothing on PATH, so the
//! action links `~/.local/bin/prometheus` to it: to the binary inside the
//! app bundle on macOS, and to the `AppImage` itself on Linux, whose mount
//! point changes on every start; started through that link, the app runs
//! its bundled CLI instead of opening a window ([`runs_cli`]). MSIX, deb
//! and rpm installs put the CLI on PATH themselves and offer no action.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::update_install::Target;

/// The names a link to the `AppImage` runs the CLI under.
const CLI_NAMES: [&str; 2] = ["prometheus", "pmt"];

/// What `~/.local/bin/prometheus` points at for the app at `exe` (given the
/// `APPIMAGE` variable) whose bundled CLI is `bundled`; `None` when this
/// install puts the CLI on PATH itself or is a development build.
#[must_use]
pub fn link_target(exe: &Path, appimage: Option<&Path>, bundled: &Path) -> Option<PathBuf> {
    match Target::detect(exe, appimage)? {
        Target::AppBundle(_) => Some(bundled.to_owned()),
        Target::AppImage(image) => Some(image),
    }
}

/// Whether the `AppImage` was started through a link named like the CLI
/// (the `AppImage` runtime passes the name it was started as in `ARGV0`).
#[must_use]
pub fn runs_cli(argv0: Option<&OsStr>) -> bool {
    argv0
        .map(Path::new)
        .and_then(Path::file_name)
        .and_then(OsStr::to_str)
        .is_some_and(|name| CLI_NAMES.contains(&name))
}

/// A link the action made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Installed {
    /// `~/.local/bin/prometheus`.
    pub link: PathBuf,
    /// What it points at.
    pub target: PathBuf,
    /// Whether `~/.local/bin` is on `PATH` (otherwise the window says to
    /// add it).
    pub on_path: bool,
}

/// Links `<user_home>/.local/bin/prometheus` to `target`. A link left by an
/// earlier install of the app (pointing at it, or at nothing any more) is
/// replaced; anything else there is another install of the CLI and stays.
///
/// # Errors
///
/// Why the link was not made.
pub fn install(
    user_home: &Path,
    target: &Path,
    path_var: Option<&OsStr>,
) -> Result<Installed, String> {
    let dir = user_home.join(".local").join("bin");
    let link = dir.join("prometheus");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    if let Ok(meta) = std::fs::symlink_metadata(&link) {
        let ours = meta.file_type().is_symlink()
            && (std::fs::read_link(&link).is_ok_and(|to| to == target) || !link.exists());
        if !ours {
            return Err(format!(
                "{} is another install of the command-line tool; remove it to link this app's",
                link.display()
            ));
        }
        std::fs::remove_file(&link).map_err(|e| format!("{}: {e}", link.display()))?;
    }
    symlink(target, &link).map_err(|e| format!("{}: {e}", link.display()))?;
    let on_path = path_var.is_some_and(|p| std::env::split_paths(p).any(|d| d == dir));
    Ok(Installed {
        link,
        target: target.to_owned(),
        on_path,
    })
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(not(unix))]
fn symlink(_target: &Path, _link: &Path) -> std::io::Result<()> {
    Err(std::io::Error::other(
        "the command-line tool is on PATH through the package's execution aliases",
    ))
}

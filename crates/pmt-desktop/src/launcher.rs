//! The Linux XDG launcher entry (spec: desktop-app, Desktop launch
//! options: `desktop.manage_launcher_entry`). It makes the app show in the
//! application menu and registers it for `prometheus://` links. Managed
//! (the default), the entry is rewritten whenever it differs from what this
//! install needs (an `AppImage` that moved, say); unmanaged, an existing
//! entry is left exactly as the user edited it and only a missing one is
//! created.

use std::path::{Path, PathBuf};

/// The entry's file name under `applications/`.
pub const FILE: &str = "prometheus-desktop.desktop";

/// `$XDG_DATA_HOME/applications/<FILE>` (default
/// `~/.local/share/applications`).
#[must_use]
pub fn path(user_home: &Path, xdg_data_home: Option<&Path>) -> PathBuf {
    xdg_data_home
        .map_or_else(|| user_home.join(".local/share"), Path::to_path_buf)
        .join("applications")
        .join(FILE)
}

/// The entry for the executable `exec`.
#[must_use]
pub fn entry(exec: &Path) -> String {
    let exec = exec.display().to_string().replace('"', "\\\"");
    format!(
        "[Desktop Entry]\nType=Application\nName=Prometheus\nComment=Prometheus agent\nExec=\"{exec}\" %u\nIcon=prometheus-desktop\nTerminal=false\nCategories=Development;Utility;\nMimeType=x-scheme-handler/prometheus;\nStartupWMClass=prometheus-desktop\n"
    )
}

/// What [`ensure`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ensured {
    /// There was none: written.
    Created,
    /// It differed and is managed: rewritten.
    Updated,
    /// Left as it was.
    Kept,
}

/// Writes the entry at `at` for `exec` as `manage` says.
///
/// # Errors
///
/// When the entry cannot be written.
pub fn ensure(at: &Path, exec: &Path, manage: bool) -> std::io::Result<Ensured> {
    let wanted = entry(exec);
    let current = std::fs::read_to_string(at).ok();
    let outcome = match current {
        None => Ensured::Created,
        Some(text) if !manage || text == wanted => return Ok(Ensured::Kept),
        Some(_) => Ensured::Updated,
    };
    if let Some(dir) = at.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(at, wanted)?;
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{Ensured, ensure, path};

    #[test]
    fn an_unmanaged_entry_is_kept_and_a_missing_one_created() {
        let dir = tempfile::tempdir().unwrap();
        let at = path(dir.path(), None);
        let exec = Path::new("/opt/Prometheus.AppImage");
        assert_eq!(ensure(&at, exec, false).unwrap(), Ensured::Created);
        std::fs::write(&at, "[Desktop Entry]\nName=Mine\n").unwrap();
        assert_eq!(ensure(&at, exec, false).unwrap(), Ensured::Kept);
        assert_eq!(
            std::fs::read_to_string(&at).unwrap(),
            "[Desktop Entry]\nName=Mine\n"
        );
        assert_eq!(ensure(&at, exec, true).unwrap(), Ensured::Updated);
        assert!(
            std::fs::read_to_string(&at)
                .unwrap()
                .contains("x-scheme-handler/prometheus")
        );
        assert_eq!(ensure(&at, exec, true).unwrap(), Ensured::Kept);
    }
}

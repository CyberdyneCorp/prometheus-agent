//! Which `NeMo` Relay `plugins.toml` files apply (spec: observability, `NeMo`
//! Relay and Langfuse integrations): Relay's user file
//! (`~/.config/nemo-relay/plugins.toml`), replaced by
//! `observability.nemo_relay.plugins_toml` when set, then the system file
//! (`/etc/nemo-relay/plugins.toml`, `%ProgramData%\nemo-relay\plugins.toml`
//! on Windows), which has the highest precedence. Relay itself reads them;
//! Prometheus only names them (`prometheus doctor observability`) and
//! passes the replacement.

use std::path::{Path, PathBuf};

/// Where a Relay file comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RelaySource {
    /// Relay's per-user file.
    User,
    /// `observability.nemo_relay.plugins_toml`, in place of the user file.
    Configured,
    /// The machine-wide file (highest precedence).
    System,
}

/// One Relay configuration file, lowest precedence first.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RelayFile {
    /// Its path.
    pub path: PathBuf,
    /// Where it comes from.
    pub source: RelaySource,
    /// It exists (only existing files apply).
    pub exists: bool,
}

/// The system file of this OS (`program_data`: `%ProgramData%` on
/// Windows).
#[must_use]
pub fn system_file(program_data: Option<&Path>) -> PathBuf {
    if cfg!(windows) {
        program_data
            .unwrap_or(Path::new(r"C:\ProgramData"))
            .join("nemo-relay")
            .join("plugins.toml")
    } else {
        PathBuf::from("/etc/nemo-relay/plugins.toml")
    }
}

/// The Relay files of a user, lowest precedence first: the configured
/// file or the user's, then `system`.
#[must_use]
pub fn files(user_home: &Path, configured: Option<&Path>, system: &Path) -> Vec<RelayFile> {
    let (path, source) = match configured {
        Some(path) => (path.to_path_buf(), RelaySource::Configured),
        None => (
            user_home.join(".config/nemo-relay/plugins.toml"),
            RelaySource::User,
        ),
    };
    [(path, source), (system.to_path_buf(), RelaySource::System)]
        .into_iter()
        .map(|(path, source)| RelayFile {
            exists: path.is_file(),
            path,
            source,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_configured_file_replaces_the_users_and_the_system_file_comes_last() {
        let home = tempfile::tempdir().unwrap();
        let user = home.path().join(".config/nemo-relay/plugins.toml");
        std::fs::create_dir_all(user.parent().unwrap()).unwrap();
        std::fs::write(&user, "version = 1\n").unwrap();
        let system = home.path().join("system.toml");
        let listed = files(home.path(), None, &system);
        assert_eq!(listed[0].source, RelaySource::User);
        assert!(listed[0].exists);
        assert_eq!(listed[1].source, RelaySource::System);
        assert!(!listed[1].exists);
        let mine = home.path().join("mine.toml");
        let listed = files(home.path(), Some(&mine), &system);
        assert_eq!(listed[0].path, mine);
        assert_eq!(listed[0].source, RelaySource::Configured);
    }
}

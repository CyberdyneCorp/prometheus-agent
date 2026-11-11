//! The global emergency pause (spec: messaging-gateway, Global emergency
//! pause): the sentinel `<home>/run/paused` (mode 0600) holding the reason,
//! who paused and when. While it exists nothing starts new work on any
//! profile of the home; turns already running finish. It is a file, not
//! daemon memory, so a pause survives daemon restarts and every process of
//! the home (daemon, in-process chat, schedulers) sees the same state.

use std::io;
use std::path::{Path, PathBuf};

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::error::{Error, codes};
use crate::private_fs;
use crate::runtime::run_dir;

/// The sentinel's file name under `<home>/run/`.
pub const FILE: &str = "paused";

/// An active pause.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pause {
    /// Why (shown in every refusal).
    pub reason: String,
    /// Who paused (`cli:<user>`, `<platform>:<sender>`).
    pub actor: String,
    /// When, UTC (RFC 3339).
    pub at: String,
}

impl Pause {
    /// What a refused submission answers, in the profile's language.
    #[must_use]
    pub fn notice(&self, text: &dyn crate::text::Catalog) -> String {
        text.text("gateway.paused_refused", &[("reason", &self.reason)])
    }

    /// The refusal as an error (`pause.active`, exit 69; English, like
    /// every error message).
    #[must_use]
    pub fn error(&self) -> Error {
        Error::new(
            &codes::PAUSE_ACTIVE,
            format!(
                "Paused: {}. Run `prometheus pause off` (or /pause off) to resume.",
                self.reason
            ),
        )
    }
}

/// The reason recorded when none is given.
pub const DEFAULT_REASON: &str = "emergency pause";

fn path(home: &Path) -> PathBuf {
    run_dir(home).join(FILE)
}

/// The active pause of `home`, if any. A sentinel that cannot be parsed
/// still pauses (with a generic reason): failing open would start work the
/// operator meant to stop.
#[must_use]
pub fn active(home: &Path) -> Option<Pause> {
    let file = path(home);
    match private_fs::read_no_follow(&file) {
        Ok(text) => Some(serde_json::from_str(&text).unwrap_or_else(|_| Pause {
            reason: DEFAULT_REASON.to_owned(),
            actor: "unknown".to_owned(),
            at: String::new(),
        })),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        // Unreadable: still paused, for the same reason.
        Err(_) => Some(Pause {
            reason: DEFAULT_REASON.to_owned(),
            actor: "unknown".to_owned(),
            at: String::new(),
        }),
    }
}

/// `Err(pause.active)` while `home` is paused.
///
/// # Errors
///
/// `pause.active` (exit 69) naming the reason.
pub fn check(home: &Path) -> Result<(), Error> {
    active(home).map_or(Ok(()), |pause| Err(pause.error()))
}

/// Pauses `home` (replacing an earlier pause's reason).
///
/// # Errors
///
/// `home.io` when the sentinel cannot be written.
pub fn pause(home: &Path, reason: Option<&str>, actor: &str) -> Result<Pause, Error> {
    let reason = reason
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .unwrap_or(DEFAULT_REASON);
    let pause = Pause {
        reason: reason.to_owned(),
        actor: actor.to_owned(),
        at: Timestamp::now().to_string(),
    };
    let dir = run_dir(home);
    private_fs::create_dir(&dir).map_err(|e| Error::io(&codes::HOME_IO, &dir, &e))?;
    let file = path(home);
    let text = serde_json::to_string_pretty(&pause)
        .map_err(|e| Error::new(&codes::HOME_IO, format!("cannot encode the pause: {e}")))?;
    private_fs::write_atomic(&file, text.as_bytes())
        .map_err(|e| Error::io(&codes::HOME_IO, &file, &e))?;
    Ok(pause)
}

/// Lifts the pause; `false` when none was active.
///
/// # Errors
///
/// `home.io` when the sentinel cannot be removed.
pub fn resume(home: &Path) -> Result<bool, Error> {
    let file = path(home);
    match std::fs::remove_file(&file) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(Error::io(&codes::HOME_IO, &file, &e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pause_is_a_file_any_process_of_the_home_reads() {
        let home = tempfile::tempdir().unwrap();
        assert!(active(home.path()).is_none());
        pause(home.path(), Some("billing incident"), "cli:alice").unwrap();
        let read = active(home.path()).unwrap();
        assert_eq!(read.reason, "billing incident");
        assert_eq!(read.actor, "cli:alice");
        assert_eq!(
            check(home.path()).unwrap_err().message(),
            "Paused: billing incident. Run `prometheus pause off` (or /pause off) to resume."
        );
        assert!(resume(home.path()).unwrap());
        assert!(!resume(home.path()).unwrap());
        assert!(check(home.path()).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn the_sentinel_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let home = tempfile::tempdir().unwrap();
        pause(home.path(), None, "cli").unwrap();
        let mode = std::fs::metadata(path(home.path()))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(active(home.path()).unwrap().reason, DEFAULT_REASON);
    }

    #[test]
    fn an_unreadable_sentinel_still_pauses() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(run_dir(home.path())).unwrap();
        std::fs::write(path(home.path()), "not json").unwrap();
        assert_eq!(active(home.path()).unwrap().reason, DEFAULT_REASON);
    }
}

//! Single wake listener ownership across surfaces (spec: voice-mode,
//! Single wake listener ownership across surfaces). One listener holds
//! the microphone per machine: the lock `run/wake.lock` of the home names
//! the owning surface, profile and process. Ownership is sticky until the
//! owner releases it, disconnects or exits (a lock whose process is gone
//! is stale); another surface never takes over by itself, and asking
//! names the owner.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use pmt_config::schema::WakeSurface;
use serde::{Deserialize, Serialize};

/// The lock's file name under the home's `run/`.
pub const FILE: &str = "wake.lock";

/// Who holds the wake listener.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Owner {
    /// `cli`, `tui` or `gui`.
    pub surface: String,
    /// The profile listening.
    pub profile: String,
    /// The owning process.
    pub pid: u32,
    /// Its start time (a reused pid is not the owner).
    pub start_time: u64,
}

impl Owner {
    /// This process as the owner on `surface` for `profile`.
    #[must_use]
    pub fn this(surface: &str, profile: &str) -> Self {
        let pid = std::process::id();
        Self {
            surface: surface.to_owned(),
            profile: profile.to_owned(),
            pid,
            start_time: pmt_core::runtime::process_start_time(pid).unwrap_or(0),
        }
    }

    /// Whether the owning process still runs.
    #[must_use]
    pub fn alive(&self) -> bool {
        pmt_core::runtime::process_start_time(self.pid) == Some(self.start_time)
    }

    /// How `/wake status` names it.
    #[must_use]
    pub fn describe(&self) -> String {
        format!(
            "the {} (process {}, profile {})",
            surface_name(&self.surface),
            self.pid,
            self.profile
        )
    }
}

/// A surface's name in messages.
#[must_use]
pub fn surface_name(surface: &str) -> &str {
    match surface {
        "cli" => "CLI",
        "tui" => "TUI",
        "gui" => "desktop app",
        other => other,
    }
}

/// `<home>/run/wake.lock`.
#[must_use]
pub fn path(home: &Path) -> PathBuf {
    pmt_core::runtime::run_dir(home).join(FILE)
}

/// The live owner, if any (a stale lock reads as none).
#[must_use]
pub fn current(home: &Path) -> Option<Owner> {
    let text = std::fs::read_to_string(path(home)).ok()?;
    serde_json::from_str::<Owner>(&text)
        .ok()
        .filter(Owner::alive)
}

/// Why a surface may not listen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// Another listener holds the microphone.
    Owned(Owner),
    /// `wake.surface` names another surface.
    NotThisSurface(&'static str),
    /// The lock could not be written.
    Io(String),
}

impl Refused {
    /// The line the surface shows.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::Owned(owner) => format!(
                "the wake listener is owned by {}; stop it there first",
                owner.describe()
            ),
            Self::NotThisSurface(surface) => format!(
                "wake.surface is '{surface}': the wake listener runs on the {} only",
                surface_name(surface)
            ),
            Self::Io(why) => format!("the wake lock could not be taken: {why}"),
        }
    }
}

/// Takes the listener for `me`; a lock already held by `me`'s process is
/// kept.
///
/// # Errors
///
/// [`Refused`]: owned by another live process, `wake.surface` names
/// another surface, or the lock cannot be written.
pub fn acquire(home: &Path, me: &Owner, configured: WakeSurface) -> Result<(), Refused> {
    if configured != WakeSurface::Auto && configured.as_str() != me.surface {
        return Err(Refused::NotThisSurface(configured.as_str()));
    }
    let file = path(home);
    if let Some(parent) = file.parent() {
        pmt_core::private_fs::create_dir(parent).map_err(|e| Refused::Io(e.to_string()))?;
    }
    for _ in 0..2 {
        match pmt_core::private_fs::new_file_options()
            .write(true)
            .create_new(true)
            .open(&file)
        {
            Ok(mut handle) => {
                let text = serde_json::to_string(me).map_err(|e| Refused::Io(e.to_string()))?;
                return handle
                    .write_all(text.as_bytes())
                    .map_err(|e| Refused::Io(e.to_string()));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                match current(home) {
                    Some(owner) if owner.pid == me.pid && owner.start_time == me.start_time => {
                        return Ok(());
                    }
                    Some(owner) => return Err(Refused::Owned(owner)),
                    // Stale: its process is gone.
                    None => {
                        let _ = std::fs::remove_file(&file);
                    }
                }
            }
            Err(e) => return Err(Refused::Io(e.to_string())),
        }
    }
    Err(Refused::Io("the lock kept changing".to_owned()))
}

/// Gives the listener up, when `me`'s process holds it.
pub fn release(home: &Path, me: &Owner) {
    if let Some(owner) = current(home)
        && owner.pid == me.pid
        && owner.start_time == me.start_time
    {
        let _ = std::fs::remove_file(path(home));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_surface_does_not_steal_the_microphone() {
        let home = tempfile::tempdir().unwrap();
        let tui = Owner::this("tui", "default");
        acquire(home.path(), &tui, WakeSurface::Auto).unwrap();
        // Another process (here: the same pid with another start time
        // stands for it) asking from the CLI.
        let cli = Owner {
            surface: "cli".to_owned(),
            start_time: tui.start_time + 1,
            ..tui.clone()
        };
        let refused = acquire(home.path(), &cli, WakeSurface::Auto).unwrap_err();
        assert_eq!(refused, Refused::Owned(tui.clone()));
        assert!(
            refused.message().contains("the TUI (process"),
            "{}",
            refused.message()
        );
        assert_eq!(current(home.path()), Some(tui.clone()));
        // The owner may arm again; releasing frees it for the CLI.
        acquire(home.path(), &tui, WakeSurface::Auto).unwrap();
        release(home.path(), &tui);
        assert_eq!(current(home.path()), None);
    }

    #[test]
    fn a_dead_owner_leaves_a_stale_lock_anyone_may_take() {
        let home = tempfile::tempdir().unwrap();
        let gone = Owner {
            surface: "gui".to_owned(),
            profile: "default".to_owned(),
            pid: u32::MAX - 7,
            start_time: 1,
        };
        std::fs::create_dir_all(home.path().join("run")).unwrap();
        std::fs::write(path(home.path()), serde_json::to_string(&gone).unwrap()).unwrap();
        assert_eq!(current(home.path()), None);
        let cli = Owner::this("cli", "default");
        acquire(home.path(), &cli, WakeSurface::Auto).unwrap();
        assert_eq!(current(home.path()), Some(cli));
    }

    #[test]
    fn a_configured_surface_is_the_only_one_that_listens() {
        let home = tempfile::tempdir().unwrap();
        let cli = Owner::this("cli", "default");
        let refused = acquire(home.path(), &cli, WakeSurface::Gui).unwrap_err();
        assert!(
            refused.message().contains("desktop app only"),
            "{}",
            refused.message()
        );
    }
}

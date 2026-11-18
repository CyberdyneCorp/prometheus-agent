//! The bot screen takeover lease (spec: execution-environments, Bot screen
//! takeover lease).
//!
//! A viewer takes the profile's bot screen over (`screen.takeover`) and
//! hands it back (`screen.handback`); while a person holds the lease, the
//! profile's browser and `computer_use` tools are refused with
//! `human_has_control`, captures included. The lease lives in
//! `<profile>/run/screen-lease.json` so every process of the profile (the
//! daemon, an in-process chat, the CLI's `screen stop --force`) sees the
//! same one; writes go under a lock beside it.
//!
//! Every takeover bumps the lease epoch: a tool call remembers the epoch it
//! started under and its result is voided when the epoch moved while it
//! ran (a takeover happened mid-call). The most recent takeover wins; a
//! viewer whose connection drops keeps the lease; a lease unused for
//! [`IDLE_WINDOWS`] idle windows of [`IDLE_WINDOW`], and at least
//! [`MIN_IDLE`], expires back to the agent.
//!
//! The lease is a tool-level fence, not an OS boundary: the screen, the X
//! display and the browser's debugging port stay reachable to any process
//! of the gateway's OS user (`SECURITY.md`).

use std::path::{Path, PathBuf};
use std::time::Duration;

use pmt_core::error::{Error, codes};
use serde::{Deserialize, Serialize};

/// One idle window: how long a viewer may stay without input before it
/// counts as idle.
pub const IDLE_WINDOW: Duration = Duration::from_secs(150);
/// Idle windows after which an unused lease expires.
pub const IDLE_WINDOWS: u32 = 4;
/// The shortest time an unused lease is kept.
pub const MIN_IDLE: Duration = Duration::from_mins(10);

/// How long a lease nobody used is kept.
#[must_use]
pub fn expiry() -> Duration {
    (IDLE_WINDOW * IDLE_WINDOWS).max(MIN_IDLE)
}

/// The person holding the screen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Holder {
    /// Who took it (a connection's principal or device name).
    pub viewer: String,
    /// When it was taken (Unix seconds).
    pub since: i64,
    /// The viewer's last input or refresh (Unix seconds).
    pub used_at: i64,
}

/// The lease as recorded.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    /// Bumped by every takeover.
    pub epoch: u64,
    /// The person holding the screen, if any.
    pub holder: Option<Holder>,
}

impl State {
    /// This state with an expired holder dropped.
    fn current(mut self, now: i64) -> Self {
        let limit = i64::try_from(expiry().as_secs()).unwrap_or(i64::MAX);
        if self
            .holder
            .as_ref()
            .is_some_and(|h| now.saturating_sub(h.used_at) >= limit)
        {
            self.holder = None;
        }
        self
    }
}

/// A hand-back that was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// Someone else holds the lease (only `force` takes it from them).
    HeldBy(String),
}

/// The lease file of one profile.
#[derive(Debug, Clone)]
pub struct ScreenLease {
    path: PathBuf,
}

/// The current time as Unix seconds.
#[must_use]
pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

impl ScreenLease {
    /// The lease of the profile at `profile_dir`.
    #[must_use]
    pub fn of(profile_dir: &Path) -> Self {
        Self {
            path: profile_dir.join("run").join("screen-lease.json"),
        }
    }

    /// The lease as of `now` (an expired holder is gone).
    #[must_use]
    pub fn read(&self, now: i64) -> State {
        let recorded: State = pmt_core::private_fs::read_no_follow(&self.path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        recorded.current(now)
    }

    /// `viewer` takes the screen over (the most recent takeover wins).
    ///
    /// # Errors
    ///
    /// `home.io` when the lease cannot be written.
    pub fn take(&self, viewer: &str, now: i64) -> Result<State, Error> {
        self.update(
            |state| {
                state.epoch += 1;
                state.holder = Some(Holder {
                    viewer: viewer.to_owned(),
                    since: now,
                    used_at: now,
                });
                Ok(())
            },
            now,
        )
    }

    /// Hands the screen back to the agent: by its holder, or by anyone
    /// with `force`. Handing back a lease nobody holds succeeds.
    ///
    /// # Errors
    ///
    /// `Ok(Err(Refused))` when someone else holds it; `home.io`.
    pub fn hand_back(
        &self,
        viewer: &str,
        force: bool,
        now: i64,
    ) -> Result<Result<State, Refused>, Error> {
        let mut refused = None;
        let state = self.update(
            |state| {
                match &state.holder {
                    Some(holder) if holder.viewer != viewer && !force => {
                        refused = Some(Refused::HeldBy(holder.viewer.clone()));
                    }
                    _ => state.holder = None,
                }
                Ok(())
            },
            now,
        )?;
        Ok(refused.map_or(Ok(state), Err))
    }

    /// The holder's input keeps the lease from expiring.
    ///
    /// # Errors
    ///
    /// `home.io`.
    pub fn touch(&self, viewer: &str, now: i64) -> Result<(), Error> {
        self.update(
            |state| {
                if let Some(holder) = state.holder.as_mut().filter(|h| h.viewer == viewer) {
                    holder.used_at = now;
                }
                Ok(())
            },
            now,
        )
        .map(|_| ())
    }

    /// Input on the screen's display stream: whoever holds the lease is
    /// using it.
    ///
    /// # Errors
    ///
    /// `home.io`.
    pub fn touch_holder(&self, now: i64) -> Result<(), Error> {
        self.update(
            |state| {
                if let Some(holder) = state.holder.as_mut() {
                    holder.used_at = now;
                }
                Ok(())
            },
            now,
        )
        .map(|_| ())
    }

    /// The agent may act: the epoch its call runs under.
    ///
    /// # Errors
    ///
    /// `screen.human_has_control` while a person holds the lease.
    pub fn guard(&self, now: i64) -> Result<u64, Error> {
        let state = self.read(now);
        match state.holder {
            Some(holder) => Err(human_has_control(&holder.viewer)),
            None => Ok(state.epoch),
        }
    }

    /// Whether a call that started under `epoch` may still deliver its
    /// result: no takeover happened since and nobody holds the lease.
    ///
    /// # Errors
    ///
    /// `screen.human_has_control` naming why the result is void.
    pub fn still(&self, epoch: u64, now: i64) -> Result<(), Error> {
        let state = self.read(now);
        if state.epoch == epoch && state.holder.is_none() {
            return Ok(());
        }
        let viewer = state
            .holder
            .map_or_else(|| "a viewer".to_owned(), |h| h.viewer);
        Err(Error::new(
            &codes::SCREEN_HUMAN_HAS_CONTROL,
            format!("{viewer} took the bot screen over while this call ran; its result is void"),
        ))
    }

    fn update(
        &self,
        change: impl FnOnce(&mut State) -> Result<(), Error>,
        now: i64,
    ) -> Result<State, Error> {
        let io = |e: std::io::Error| Error::io(&codes::HOME_IO, &self.path, &e);
        if let Some(dir) = self.path.parent() {
            pmt_core::private_fs::create_dir(dir).map_err(io)?;
        }
        let _lock = pmt_core::private_fs::lock_beside(&self.path).map_err(io)?;
        let mut state = self.read(now);
        change(&mut state)?;
        let bytes = serde_json::to_vec(&state).expect("a lease serializes");
        pmt_core::private_fs::write_atomic(&self.path, &bytes).map_err(io)?;
        Ok(state)
    }
}

/// Marks the profile's bot screen as used now, when one runs (the
/// supervisor created its `run/screen/used` marker): its idle stop counts
/// from the last use.
pub fn mark_screen_used(profile_dir: &Path) {
    let marker = profile_dir.join("run").join("screen").join("used");
    if let Ok(file) = std::fs::File::options().write(true).open(marker) {
        let _ = file.set_modified(std::time::SystemTime::now());
    }
}

/// The refusal while `viewer` holds the screen.
#[must_use]
pub fn human_has_control(viewer: &str) -> Error {
    Error::new(
        &codes::SCREEN_HUMAN_HAS_CONTROL,
        format!("{viewer} has taken the bot screen over; wait until they hand it back"),
    )
    .with_hint("the person hands it back from the desktop app (or `screen.handback`)")
}

/// The tool failure kind the spec names.
pub const FAILURE_KIND: &str = "human_has_control";

#[cfg(test)]
mod tests {
    use super::*;

    fn lease() -> (tempfile::TempDir, ScreenLease) {
        let dir = tempfile::tempdir().unwrap();
        let lease = ScreenLease::of(dir.path());
        (dir, lease)
    }

    #[test]
    fn the_agent_waits_while_a_person_holds_the_screen() {
        let (_dir, lease) = lease();
        let epoch = lease.guard(100).unwrap();
        lease.take("desktop", 101).unwrap();
        let refused = lease.guard(102).unwrap_err();
        assert_eq!(refused.code(), &codes::SCREEN_HUMAN_HAS_CONTROL);
        // A call that started before the takeover is voided.
        assert!(lease.still(epoch, 103).is_err());
        lease.hand_back("desktop", false, 104).unwrap().unwrap();
        // Handed back: new calls run, the old one stays void (epoch moved).
        let after = lease.guard(105).unwrap();
        assert_ne!(after, epoch);
        assert!(lease.still(epoch, 106).is_err());
        assert!(lease.still(after, 106).is_ok());
    }

    #[test]
    fn the_most_recent_takeover_wins_and_only_force_takes_it_from_them() {
        let (_dir, lease) = lease();
        lease.take("laptop", 10).unwrap();
        lease.take("phone", 11).unwrap();
        assert_eq!(lease.read(12).holder.unwrap().viewer, "phone");
        assert_eq!(
            lease.hand_back("laptop", false, 13).unwrap(),
            Err(Refused::HeldBy("phone".to_owned()))
        );
        assert!(lease.hand_back("laptop", true, 14).unwrap().is_ok());
        assert!(lease.guard(15).is_ok());
    }

    #[test]
    fn an_unused_lease_expires_after_four_idle_windows_and_ten_minutes() {
        let (_dir, lease) = lease();
        let limit = i64::try_from(expiry().as_secs()).unwrap();
        assert!(limit >= 600);
        lease.take("desktop", 0).unwrap();
        lease.touch("desktop", 100).unwrap();
        assert!(lease.guard(100 + limit - 1).is_err());
        assert!(lease.guard(100 + limit).is_ok());
    }
}

//! First-run onboarding progress and session launch flags (spec: cli,
//! Onboarding state; Ignore profile config and rules). Both live in the
//! store, never in `config.toml`.

use pmt_core::error::Error;
use pmt_core::id::SessionId;
use rusqlite::params;

use crate::error::sql;
use crate::sessions::{self, now_ms};
use crate::store::Store;

/// What an onboarding record marks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Setup was offered at an interactive start.
    SetupOffered,
    /// A one-time tip was shown.
    Tip,
    /// A tour step was completed.
    Tour,
}

impl Step {
    const fn as_str(self) -> &'static str {
        match self {
            Self::SetupOffered => "setup_offered",
            Self::Tip => "tip",
            Self::Tour => "tour",
        }
    }
}

/// A flag a session was launched with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchFlag {
    /// `--ignore-user-config`.
    IgnoreUserConfig,
    /// `--ignore-rules`.
    IgnoreRules,
}

impl LaunchFlag {
    /// The stored name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::IgnoreUserConfig => "ignore_user_config",
            Self::IgnoreRules => "ignore_rules",
        }
    }
}

impl Store {
    /// Records `id` of `step` once; returns whether it was new (so a tip is
    /// shown only by the caller that recorded it).
    ///
    /// # Errors
    ///
    /// `store.busy`, `store.sqlite`.
    pub fn mark_onboarding(&self, step: Step, id: &str) -> Result<bool, Error> {
        self.write(|tx| {
            tx.execute(
                "INSERT OR IGNORE INTO onboarding (kind, id, at) VALUES (?1, ?2, ?3)",
                params![step.as_str(), id, now_ms()],
            )
            .map(|n| n == 1)
            .map_err(sql)
        })
    }

    /// Whether `id` of `step` is recorded.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn onboarding_done(&self, step: Step, id: &str) -> Result<bool, Error> {
        self.read(|conn| {
            conn.query_row(
                "SELECT count(*) FROM onboarding WHERE kind = ?1 AND id = ?2",
                params![step.as_str(), id],
                |row| row.get::<_, i64>(0),
            )
            .map(|n| n > 0)
            .map_err(sql)
        })
    }

    /// Clears every onboarding record (`setup --reset-onboarding`);
    /// returns how many were removed.
    ///
    /// # Errors
    ///
    /// `store.busy`, `store.sqlite`.
    pub fn reset_onboarding(&self) -> Result<usize, Error> {
        self.write(|tx| tx.execute("DELETE FROM onboarding", []).map_err(sql))
    }

    /// Records the launch flags of `session`.
    ///
    /// # Errors
    ///
    /// `session.not_found`, `store.busy`, `store.sqlite`.
    pub fn set_launch_flags(&self, session: SessionId, flags: &[LaunchFlag]) -> Result<(), Error> {
        self.write(|tx| {
            sessions::load(tx, session)?.ok_or_else(|| sessions::not_found(session))?;
            for flag in flags {
                tx.execute(
                    "INSERT OR IGNORE INTO session_launch_flags (session_id, flag) VALUES (?1, ?2)",
                    params![session.to_string(), flag.as_str()],
                )
                .map_err(sql)?;
            }
            Ok(())
        })
    }

    /// The launch flags `session` was started with, by stored name.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn launch_flags(&self, session: SessionId) -> Result<Vec<String>, Error> {
        self.read(|conn| {
            let mut statement = conn
                .prepare(
                    "SELECT flag FROM session_launch_flags WHERE session_id = ?1 ORDER BY flag",
                )
                .map_err(sql)?;
            let rows = statement
                .query_map([session.to_string()], |row| row.get::<_, String>(0))
                .map_err(sql)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(sql)
        })
    }
}

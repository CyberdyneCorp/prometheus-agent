//! Per-skill usage records (spec: skills, Skill usage records).
//!
//! One row per skill name of the profile. Every counter bump is one
//! statement (insert-or-update) so concurrent sessions of a profile never
//! lose an increment; the record stays on this machine.

use jiff::Timestamp;
use pmt_core::error::Error;
use rusqlite::{OptionalExtension, Row, params};

use crate::error::{corrupt, sql};
pub use crate::model::{CreatedBy, SkillState};
use crate::sessions::now_ms;
use crate::store::Store;

/// What happened to a skill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillActivity {
    /// Loaded by a slash command, preload or bundle (`use_count`).
    Used,
    /// Loaded with `skill_view` (`view_count`).
    Viewed,
    /// Written by a successful `skill_manage` call (`patch_count`).
    Patched,
}

impl SkillActivity {
    const fn column(self) -> &'static str {
        match self {
            Self::Used => "use_count",
            Self::Viewed => "view_count",
            Self::Patched => "patch_count",
        }
    }
}

/// One skill's record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillUsage {
    /// The skill name.
    pub name: String,
    /// Slash, preload and bundle loads.
    pub use_count: u64,
    /// `skill_view` loads.
    pub view_count: u64,
    /// Successful `skill_manage` writes.
    pub patch_count: u64,
    /// The last of those.
    pub last_activity_at: Option<Timestamp>,
    /// Lifecycle state.
    pub state: SkillState,
    /// Pinned skills are never deleted or archived by the agent.
    pub pinned: bool,
    /// Who created it.
    pub created_by: CreatedBy,
}

const COLUMNS: &str =
    "name, use_count, view_count, patch_count, last_activity_at, state, pinned, created_by";

fn count(row: &Row<'_>, index: usize, column: &str) -> Result<u64, Error> {
    let value: i64 = row.get(index).map_err(sql)?;
    u64::try_from(value).map_err(|e| corrupt(column, e))
}

fn usage_from_row(row: &Row<'_>) -> Result<SkillUsage, Error> {
    let at: Option<i64> = row.get(4).map_err(sql)?;
    let state: String = row.get(5).map_err(sql)?;
    let created_by: String = row.get(7).map_err(sql)?;
    Ok(SkillUsage {
        name: row.get(0).map_err(sql)?,
        use_count: count(row, 1, "skill_usage.use_count")?,
        view_count: count(row, 2, "skill_usage.view_count")?,
        patch_count: count(row, 3, "skill_usage.patch_count")?,
        last_activity_at: at
            .map(|ms| {
                Timestamp::from_millisecond(ms)
                    .map_err(|e| corrupt("skill_usage.last_activity_at", e))
            })
            .transpose()?,
        state: state.parse()?,
        pinned: row.get::<_, i64>(6).map_err(sql)? != 0,
        created_by: created_by.parse()?,
    })
}

impl Store {
    /// Bumps `activity`'s counter of skill `name` and its activity time,
    /// creating the record (attributed to `created_by`) on first use.
    ///
    /// # Errors
    ///
    /// `store.sqlite`, `store.busy`.
    pub fn record_skill_activity(
        &self,
        name: &str,
        activity: SkillActivity,
        created_by: CreatedBy,
    ) -> Result<(), Error> {
        let column = activity.column();
        let statement = format!(
            "INSERT INTO skill_usage (name, {column}, last_activity_at, created_by) \
             VALUES (?1, 1, ?2, ?3) ON CONFLICT (name) DO UPDATE SET \
             {column} = {column} + 1, last_activity_at = excluded.last_activity_at"
        );
        let now = now_ms();
        self.write(|tx| {
            tx.execute(&statement, params![name, now, created_by.as_str()])
                .map_err(sql)?;
            Ok(())
        })
    }

    /// Sets whether skill `name` is pinned (creating its record).
    ///
    /// # Errors
    ///
    /// `store.sqlite`, `store.busy`.
    pub fn set_skill_pinned(&self, name: &str, pinned: bool) -> Result<(), Error> {
        self.write(|tx| {
            tx.execute(
                "INSERT INTO skill_usage (name, pinned) VALUES (?1, ?2) \
                 ON CONFLICT (name) DO UPDATE SET pinned = excluded.pinned",
                params![name, i64::from(pinned)],
            )
            .map_err(sql)?;
            Ok(())
        })
    }

    /// Sets the lifecycle state of skill `name` (creating its record).
    ///
    /// # Errors
    ///
    /// `store.sqlite`, `store.busy`.
    pub fn set_skill_state(&self, name: &str, state: SkillState) -> Result<(), Error> {
        self.write(|tx| {
            tx.execute(
                "INSERT INTO skill_usage (name, state) VALUES (?1, ?2) \
                 ON CONFLICT (name) DO UPDATE SET state = excluded.state",
                params![name, state.as_str()],
            )
            .map_err(sql)?;
            Ok(())
        })
    }

    /// The record of skill `name`, if it has one.
    ///
    /// # Errors
    ///
    /// `store.sqlite` (also for a row that does not parse).
    pub fn skill_usage(&self, name: &str) -> Result<Option<SkillUsage>, Error> {
        self.read(|conn| {
            conn.query_row(
                &format!("SELECT {COLUMNS} FROM skill_usage WHERE name = ?1"),
                [name],
                |row| Ok(usage_from_row(row)),
            )
            .optional()
            .map_err(sql)?
            .transpose()
        })
    }

    /// Every record, by name.
    ///
    /// # Errors
    ///
    /// `store.sqlite` (also for a row that does not parse).
    pub fn skill_usages(&self) -> Result<Vec<SkillUsage>, Error> {
        self.read(|conn| {
            let mut statement = conn
                .prepare(&format!("SELECT {COLUMNS} FROM skill_usage ORDER BY name"))
                .map_err(sql)?;
            let rows = statement
                .query_map([], |row| Ok(usage_from_row(row)))
                .map_err(sql)?;
            rows.map(|row| row.map_err(sql)?).collect()
        })
    }
}

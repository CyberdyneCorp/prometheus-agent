//! The tool surface each session froze (spec: tool-system, Tool
//! introspection commands; Tool surface frozen per session).
//!
//! A session records its surface when it builds it, so `tools list
//! --session <id>` answers with what that session was given even after
//! the profile's config, grants or installed runtimes changed. Rebuilding
//! the surface (compaction, `/tools reload --now`) replaces the record.

use jiff::Timestamp;
use pmt_core::error::Error;
use pmt_core::id::SessionId;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::error::{corrupt, sql};
use crate::sessions::{load, not_found};
use crate::store::Store;

/// One registered tool and the session's verdict on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurfaceTool {
    /// Tool name.
    pub name: String,
    /// Its toolset.
    pub toolset: String,
    /// Who registered it.
    pub owner: String,
    /// `exposed` or the exclusion reason code.
    pub exposure: String,
}

/// A session's recorded surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedSurface {
    /// The surface generation (bumped by each rebuild).
    pub generation: u64,
    /// When the session built it.
    pub recorded_at: Timestamp,
    /// Every registered tool, in registry order.
    pub tools: Vec<SurfaceTool>,
}

impl Store {
    /// Records (or replaces) the surface of `session`.
    ///
    /// # Errors
    ///
    /// `session.not_found`, `store.sqlite`.
    pub fn record_tool_surface(
        &self,
        session: SessionId,
        surface: &RecordedSurface,
    ) -> Result<(), Error> {
        let tools = serde_json::to_string(&surface.tools)
            .map_err(|e| corrupt("session_tool_surface.tools", e))?;
        let generation = i64::try_from(surface.generation).unwrap_or(i64::MAX);
        self.write(|tx| {
            if load(tx, session)?.is_none() {
                return Err(not_found(session));
            }
            tx.execute(
                "INSERT INTO session_tool_surface (session_id, generation, recorded_at, tools) \
                 VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT (session_id) DO UPDATE SET generation = excluded.generation, \
                 recorded_at = excluded.recorded_at, tools = excluded.tools",
                params![
                    session.to_string(),
                    generation,
                    surface.recorded_at.as_millisecond(),
                    tools
                ],
            )
            .map_err(sql)?;
            Ok(())
        })
    }

    /// The surface `session` recorded; `None` when it recorded none.
    ///
    /// # Errors
    ///
    /// `store.sqlite` (also for a record that does not parse).
    pub fn tool_surface(&self, session: SessionId) -> Result<Option<RecordedSurface>, Error> {
        let row = self.read(|conn| {
            conn.query_row(
                "SELECT generation, recorded_at, tools FROM session_tool_surface \
                 WHERE session_id = ?1",
                [session.to_string()],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(sql)
        })?;
        let Some((generation, recorded_at, tools)) = row else {
            return Ok(None);
        };
        Ok(Some(RecordedSurface {
            generation: u64::try_from(generation)
                .map_err(|e| corrupt("session_tool_surface.generation", e))?,
            recorded_at: Timestamp::from_millisecond(recorded_at)
                .map_err(|e| corrupt("session_tool_surface.recorded_at", e))?,
            tools: serde_json::from_str(&tools)
                .map_err(|e| corrupt("session_tool_surface.tools", e))?,
        }))
    }
}

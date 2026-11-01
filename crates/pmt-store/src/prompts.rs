//! The system prompt and tool schemas each session froze (spec:
//! agent-turn-loop, System prompt frozen per session).
//!
//! The turn loop builds a session's prompt once and every later turn
//! replays these bytes, so a resumed session sends exactly what it sent
//! before even when `SOUL.md`, memory or skills changed on disk. Saving a
//! prompt also sets `sessions.system_prompt_hash`.

use jiff::Timestamp;
use pmt_core::error::Error;
use pmt_core::id::SessionId;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{corrupt, sql};
use crate::sessions::{load, not_found};
use crate::store::Store;

/// One cache tier of a system prompt (`static`, `profile`, `session`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptTier {
    /// Tier name.
    pub tier: String,
    /// The tier's text, sent as one system block.
    pub text: String,
}

/// A session's frozen prompt.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionPrompt {
    /// The tiers in request order.
    pub tiers: Vec<PromptTier>,
    /// `sha256:<hex>` of the joined prompt text.
    pub sha256: String,
    /// The tool schemas in request order (a JSON array).
    pub tools: Value,
    /// Provider the prompt was built for.
    pub provider: Option<String>,
    /// Model the prompt was built for.
    pub model: Option<String>,
    /// When it was built.
    pub built_at: Timestamp,
    /// Why it was (re)built (`new_session`, `model_changed`...).
    pub reason: String,
}

impl Store {
    /// Stores (or replaces) the frozen prompt of `session` and records its
    /// hash on the session row.
    ///
    /// # Errors
    ///
    /// `session.not_found`, `store.sqlite`.
    pub fn save_session_prompt(
        &self,
        session: SessionId,
        prompt: &SessionPrompt,
    ) -> Result<(), Error> {
        let tiers = serde_json::to_string(&prompt.tiers)
            .map_err(|e| corrupt("session_prompts.tiers", e))?;
        let tools = prompt.tools.to_string();
        self.write(|tx| {
            if load(tx, session)?.is_none() {
                return Err(not_found(session));
            }
            tx.execute(
                "INSERT INTO session_prompts (session_id, tiers, sha256, tools, provider, model, \
                 built_at, reason) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
                 ON CONFLICT (session_id) DO UPDATE SET tiers = excluded.tiers, \
                 sha256 = excluded.sha256, tools = excluded.tools, provider = excluded.provider, \
                 model = excluded.model, built_at = excluded.built_at, reason = excluded.reason",
                params![
                    session.to_string(),
                    tiers,
                    prompt.sha256,
                    tools,
                    prompt.provider,
                    prompt.model,
                    prompt.built_at.as_millisecond(),
                    prompt.reason
                ],
            )
            .map_err(sql)?;
            tx.execute(
                "UPDATE sessions SET system_prompt_hash = ?2 WHERE id = ?1",
                params![session.to_string(), prompt.sha256],
            )
            .map_err(sql)?;
            Ok(())
        })
    }

    /// The prompt `session` froze; `None` when it has none yet.
    ///
    /// # Errors
    ///
    /// `store.sqlite` (also for a row that does not parse).
    pub fn session_prompt(&self, session: SessionId) -> Result<Option<SessionPrompt>, Error> {
        type Row = (
            String,
            String,
            String,
            Option<String>,
            Option<String>,
            i64,
            String,
        );
        let row: Option<Row> = self.read(|conn| {
            conn.query_row(
                "SELECT tiers, sha256, tools, provider, model, built_at, reason \
                 FROM session_prompts WHERE session_id = ?1",
                [session.to_string()],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                },
            )
            .optional()
            .map_err(sql)
        })?;
        let Some((tiers, sha256, tools, provider, model, built_at, reason)) = row else {
            return Ok(None);
        };
        Ok(Some(SessionPrompt {
            tiers: serde_json::from_str(&tiers).map_err(|e| corrupt("session_prompts.tiers", e))?,
            sha256,
            tools: serde_json::from_str(&tools).map_err(|e| corrupt("session_prompts.tools", e))?,
            provider,
            model,
            built_at: Timestamp::from_millisecond(built_at)
                .map_err(|e| corrupt("session_prompts.built_at", e))?,
            reason,
        }))
    }
}

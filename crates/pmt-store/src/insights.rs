//! What `prometheus usage insights` reads besides the ledger (spec:
//! observability, Usage insights report): the sessions active in a
//! window with the source of their lineage's root (a surface, or a
//! messaging platform), and their user messages, tool results and skill
//! loads. Nothing here calls a model.

use jiff::Timestamp;
use pmt_core::error::Error;
use pmt_core::id::SessionId;
use rusqlite::params;
use serde_json::Value;

use crate::error::sql;
use crate::model::{Origin, Source};
use crate::store::Store;

/// A session active in the window and the source its root started from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabeledSession {
    /// The session.
    pub id: SessionId,
    /// Its lineage root.
    pub root: SessionId,
    /// The root's surface (`cli`, `tui`, `cron`...) or, for a messaging
    /// conversation, its platform (`telegram`...).
    pub source: String,
}

/// What happened in a session at one moment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Activity {
    /// The person wrote.
    UserMessage,
    /// A tool answered.
    ToolResult {
        /// The tool.
        tool: String,
        /// It failed (error, blocked, denied, timeout, cancelled).
        failed: bool,
    },
    /// The agent loaded a skill (`skill_view`).
    SkillLoad {
        /// The skill.
        skill: String,
    },
}

/// One activity row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityRow {
    /// The session.
    pub session_id: SessionId,
    /// What.
    pub activity: Activity,
    /// When.
    pub at: Timestamp,
}

/// The source label of a root session.
#[must_use]
pub fn source_label(source: Source, origin: &Origin) -> String {
    match origin {
        Origin::Gateway { platform, .. } => platform.clone(),
        _ => source.as_str().to_owned(),
    }
}

impl Store {
    /// Every session with activity since `since`, labeled with its root's
    /// source.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn labeled_sessions(&self, since: Timestamp) -> Result<Vec<LabeledSession>, Error> {
        self.read(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT s.id, s.root_session_id, r.source, r.origin FROM sessions s \
                     JOIN sessions r ON r.id = s.root_session_id \
                     WHERE s.last_activity_at >= ?1 ORDER BY s.started_at, s.id",
                )
                .map_err(sql)?;
            let mut rows = stmt.query(params![since.as_millisecond()]).map_err(sql)?;
            let mut out = Vec::new();
            while let Some(row) = rows.next().map_err(sql)? {
                let text = |i: usize| row.get::<_, String>(i).map_err(sql);
                let source: Source = text(2)?.parse().unwrap_or(Source::Cli);
                let origin: Origin = serde_json::from_str(&text(3)?).unwrap_or_default();
                out.push(LabeledSession {
                    id: SessionId::parse(&text(0)?)?,
                    root: SessionId::parse(&text(1)?)?,
                    source: source_label(source, &origin),
                });
            }
            Ok(out)
        })
    }

    /// User messages, tool results and skill loads since `since`, oldest
    /// first.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn activity_since(&self, since: Timestamp) -> Result<Vec<ActivityRow>, Error> {
        self.read(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT session_id, role, tool_name, metadata, tool_calls, created_at \
                     FROM messages WHERE created_at >= ?1 \
                     AND (role IN ('user', 'tool') OR (role = 'assistant' AND tool_calls LIKE '%skill_view%')) \
                     ORDER BY created_at, seq",
                )
                .map_err(sql)?;
            let mut rows = stmt.query(params![since.as_millisecond()]).map_err(sql)?;
            let mut out = Vec::new();
            while let Some(row) = rows.next().map_err(sql)? {
                let session_id = SessionId::parse(&row.get::<_, String>(0).map_err(sql)?)?;
                let role: String = row.get(1).map_err(sql)?;
                let at = Timestamp::from_millisecond(row.get::<_, i64>(5).map_err(sql)?)
                    .unwrap_or(Timestamp::UNIX_EPOCH);
                let tool: Option<String> = row.get(2).map_err(sql)?;
                let metadata: Option<String> = row.get(3).map_err(sql)?;
                let calls: Option<String> = row.get(4).map_err(sql)?;
                for activity in activities(&role, tool, metadata.as_deref(), calls.as_deref()) {
                    out.push(ActivityRow {
                        session_id,
                        activity,
                        at,
                    });
                }
            }
            Ok(out)
        })
    }
}

/// The activities one message row stands for.
fn activities(
    role: &str,
    tool: Option<String>,
    metadata: Option<&str>,
    calls: Option<&str>,
) -> Vec<Activity> {
    match role {
        "user" => {
            let hidden = metadata
                .and_then(|m| serde_json::from_str::<Value>(m).ok())
                .is_some_and(|m| m["hidden"] == true);
            if hidden {
                Vec::new()
            } else {
                vec![Activity::UserMessage]
            }
        }
        "tool" => {
            let failed = metadata
                .and_then(|m| serde_json::from_str::<Value>(m).ok())
                .is_some_and(|m| m["is_error"] == true);
            vec![Activity::ToolResult {
                tool: tool.unwrap_or_default(),
                failed,
            }]
        }
        _ => calls
            .and_then(|c| serde_json::from_str::<Value>(c).ok())
            .and_then(|c| c.as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter(|call| call["name"] == "skill_view")
            .filter_map(|call| {
                let arguments = match &call["arguments"] {
                    Value::String(text) => serde_json::from_str::<Value>(text).ok()?,
                    other => other.clone(),
                };
                Some(Activity::SkillLoad {
                    skill: arguments["name"].as_str()?.to_owned(),
                })
            })
            .collect(),
    }
}

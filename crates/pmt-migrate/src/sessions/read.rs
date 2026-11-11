//! Reading a staged copy of a Hermes `state.db` (schemas 24 through 32).
//! Columns a schema version lacks read as NULL; only the session,
//! message and per-model usage tables are read, never Hermes' runtime
//! coordination tables (turn leases, compression locks, admissions,
//! worker executions and receipts, gateway routing, async delegations).

use std::collections::BTreeSet;
use std::path::Path;

use pmt_core::error::{Error, codes};
use rusqlite::{Connection, OpenFlags, Row};
use serde_json::Value;

/// The Hermes `state.db` schema versions the migrator reads.
pub const SUPPORTED: std::ops::RangeInclusive<i64> = 24..=32;

/// One Hermes session row.
#[derive(Debug, Clone, Default, PartialEq)]
#[allow(clippy::struct_excessive_bools)] // mirrors the row's flags
pub(super) struct HermesSession {
    pub(super) id: String,
    pub(super) source: String,
    pub(super) user_id: Option<String>,
    pub(super) session_key: Option<String>,
    pub(super) chat_id: Option<String>,
    pub(super) thread_id: Option<String>,
    pub(super) origin_json: Option<String>,
    pub(super) model: Option<String>,
    pub(super) model_config: Option<String>,
    pub(super) parent_session_id: Option<String>,
    pub(super) started_at: f64,
    pub(super) ended_at: Option<f64>,
    pub(super) end_reason: Option<String>,
    pub(super) input_tokens: i64,
    pub(super) output_tokens: i64,
    pub(super) cache_read_tokens: i64,
    pub(super) cache_write_tokens: i64,
    pub(super) reasoning_tokens: i64,
    pub(super) cwd: Option<String>,
    pub(super) git_branch: Option<String>,
    pub(super) git_repo_root: Option<String>,
    pub(super) billing_provider: Option<String>,
    pub(super) billing_mode: Option<String>,
    pub(super) estimated_cost_usd: Option<f64>,
    pub(super) actual_cost_usd: Option<f64>,
    pub(super) title: Option<String>,
    pub(super) title_source: Option<String>,
    pub(super) last_activity_at: Option<f64>,
    pub(super) archived: bool,
    pub(super) auto_archived: bool,
    pub(super) pinned: bool,
    pub(super) hidden: bool,
}

/// One Hermes message row.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct HermesMessage {
    pub(super) role: String,
    pub(super) content: Option<String>,
    pub(super) tool_call_id: Option<String>,
    pub(super) tool_calls: Option<String>,
    pub(super) tool_name: Option<String>,
    pub(super) timestamp: f64,
    pub(super) finish_reason: Option<String>,
    pub(super) reasoning: Option<String>,
    pub(super) reasoning_content: Option<String>,
    pub(super) reasoning_details: Option<String>,
    pub(super) codex_reasoning_items: Option<String>,
    pub(super) codex_message_items: Option<String>,
    pub(super) platform_message_id: Option<String>,
    pub(super) active: bool,
    pub(super) compacted: bool,
}

/// One per-model usage row.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct ModelUsage {
    pub(super) model: String,
    pub(super) billing_provider: String,
    pub(super) billing_mode: String,
    pub(super) input_tokens: i64,
    pub(super) output_tokens: i64,
    pub(super) cache_read_tokens: i64,
    pub(super) cache_write_tokens: i64,
    pub(super) reasoning_tokens: i64,
    pub(super) estimated_cost_usd: f64,
    pub(super) actual_cost_usd: f64,
    pub(super) last_seen: Option<f64>,
}

const SESSION_COLUMNS: &[&str] = &[
    "id",
    "source",
    "user_id",
    "session_key",
    "chat_id",
    "thread_id",
    "origin_json",
    "model",
    "model_config",
    "parent_session_id",
    "started_at",
    "ended_at",
    "end_reason",
    "input_tokens",
    "output_tokens",
    "cache_read_tokens",
    "cache_write_tokens",
    "reasoning_tokens",
    "cwd",
    "git_branch",
    "git_repo_root",
    "billing_provider",
    "billing_mode",
    "estimated_cost_usd",
    "actual_cost_usd",
    "title",
    "title_source",
    "last_activity_at",
    "archived",
    "auto_archived",
    "pinned",
    "hidden",
];

const MESSAGE_COLUMNS: &[&str] = &[
    "role",
    "content",
    "tool_call_id",
    "tool_calls",
    "tool_name",
    "timestamp",
    "finish_reason",
    "reasoning",
    "reasoning_content",
    "reasoning_details",
    "codex_reasoning_items",
    "codex_message_items",
    "platform_message_id",
    "active",
    "compacted",
];

const USAGE_COLUMNS: &[&str] = &[
    "model",
    "billing_provider",
    "billing_mode",
    "input_tokens",
    "output_tokens",
    "cache_read_tokens",
    "cache_write_tokens",
    "reasoning_tokens",
    "estimated_cost_usd",
    "actual_cost_usd",
    "last_seen",
];

fn sql_error(path: &Path, e: &rusqlite::Error) -> Error {
    Error::new(
        &codes::MIGRATE_COMPONENT,
        format!(
            "cannot read the Hermes state.db copy {}: {e}",
            path.display()
        ),
    )
}

/// An open staged copy.
pub(super) struct HermesDb {
    conn: Connection,
    path: std::path::PathBuf,
    session_select: String,
    message_select: String,
    usage_select: Option<String>,
}

impl HermesDb {
    /// Opens the copy at `path` and checks its schema version.
    ///
    /// # Errors
    ///
    /// `migrate.component` for an unsupported schema or an unreadable
    /// file.
    pub(super) fn open(path: &Path) -> Result<Self, Error> {
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn = Connection::open_with_flags(path, flags).map_err(|e| sql_error(path, &e))?;
        let version: Option<i64> = conn
            .query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))
            .ok()
            .flatten();
        let version = version.unwrap_or(0);
        if !SUPPORTED.contains(&version) {
            return Err(Error::new(
                &codes::MIGRATE_COMPONENT,
                format!("Hermes state.db schema {version} is not supported (24-32)"),
            ));
        }
        let session_select = select(&conn, "sessions", SESSION_COLUMNS, path)?;
        let message_select = select(&conn, "messages", MESSAGE_COLUMNS, path)?;
        let usage_select = select(&conn, "session_model_usage", USAGE_COLUMNS, path).ok();
        Ok(Self {
            conn,
            path: path.to_owned(),
            session_select,
            message_select,
            usage_select,
        })
    }

    /// Every session, oldest first.
    ///
    /// # Errors
    ///
    /// `migrate.component`.
    pub(super) fn sessions(&self) -> Result<Vec<HermesSession>, Error> {
        let sql = format!("{} ORDER BY started_at, id", self.session_select);
        self.rows(&sql, &[], session_from_row)
    }

    /// How many messages `session` has.
    ///
    /// # Errors
    ///
    /// `migrate.component`.
    pub(super) fn message_count(&self, session: &str) -> Result<u64, Error> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE session_id = ?1",
                [session],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| u64::try_from(n).unwrap_or(0))
            .map_err(|e| sql_error(&self.path, &e))
    }

    /// `session`'s messages in order.
    ///
    /// # Errors
    ///
    /// `migrate.component`.
    pub(super) fn messages(&self, session: &str) -> Result<Vec<HermesMessage>, Error> {
        let sql = format!(
            "{} WHERE session_id = ?1 ORDER BY timestamp, id",
            self.message_select
        );
        self.rows(&sql, &[session], message_from_row)
    }

    /// `session`'s per-model usage rows (none on a store without them).
    ///
    /// # Errors
    ///
    /// `migrate.component`.
    pub(super) fn model_usage(&self, session: &str) -> Result<Vec<ModelUsage>, Error> {
        let Some(select) = &self.usage_select else {
            return Ok(Vec::new());
        };
        let sql = format!("{select} WHERE session_id = ?1 ORDER BY model");
        self.rows(&sql, &[session], usage_from_row)
    }

    fn rows<T>(
        &self,
        sql: &str,
        args: &[&str],
        map: fn(&Row<'_>) -> rusqlite::Result<T>,
    ) -> Result<Vec<T>, Error> {
        let mut stmt = self
            .conn
            .prepare(sql)
            .map_err(|e| sql_error(&self.path, &e))?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(args), map)
            .map_err(|e| sql_error(&self.path, &e))?;
        rows.collect::<Result<_, _>>()
            .map_err(|e| sql_error(&self.path, &e))
    }
}

/// `SELECT <columns> FROM <table>` with NULL for the columns `table` lacks.
fn select(conn: &Connection, table: &str, columns: &[&str], path: &Path) -> Result<String, Error> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|e| sql_error(path, &e))?;
    let present: BTreeSet<String> = stmt
        .query_map([], |r| r.get::<_, String>(1))
        .map_err(|e| sql_error(path, &e))?
        .collect::<Result<_, _>>()
        .map_err(|e| sql_error(path, &e))?;
    if present.is_empty() {
        return Err(Error::new(
            &codes::MIGRATE_COMPONENT,
            format!("the Hermes state.db has no {table} table"),
        ));
    }
    let list: Vec<String> = columns
        .iter()
        .map(|c| {
            if present.contains(*c) {
                format!("\"{c}\"")
            } else {
                "NULL".to_owned()
            }
        })
        .collect();
    Ok(format!("SELECT {} FROM {table}", list.join(", ")))
}

fn text(row: &Row<'_>, i: usize) -> rusqlite::Result<Option<String>> {
    Ok(match row.get_ref(i)? {
        rusqlite::types::ValueRef::Null => None,
        rusqlite::types::ValueRef::Text(t) | rusqlite::types::ValueRef::Blob(t) => {
            Some(String::from_utf8_lossy(t).into_owned())
        }
        rusqlite::types::ValueRef::Integer(n) => Some(n.to_string()),
        rusqlite::types::ValueRef::Real(f) => Some(f.to_string()),
    })
}

fn real(row: &Row<'_>, i: usize) -> rusqlite::Result<Option<f64>> {
    Ok(match row.get_ref(i)? {
        rusqlite::types::ValueRef::Integer(n) =>
        {
            #[allow(clippy::cast_precision_loss)]
            Some(n as f64)
        }
        rusqlite::types::ValueRef::Real(f) => Some(f),
        rusqlite::types::ValueRef::Text(t) => String::from_utf8_lossy(t).parse().ok(),
        _ => None,
    })
}

fn int(row: &Row<'_>, i: usize) -> rusqlite::Result<i64> {
    #[allow(clippy::cast_possible_truncation)]
    Ok(real(row, i)?.map_or(0, |f| f as i64))
}

fn flag(row: &Row<'_>, i: usize) -> rusqlite::Result<bool> {
    Ok(int(row, i)? != 0)
}

fn session_from_row(row: &Row<'_>) -> rusqlite::Result<HermesSession> {
    Ok(HermesSession {
        id: text(row, 0)?.unwrap_or_default(),
        source: text(row, 1)?.unwrap_or_default(),
        user_id: text(row, 2)?,
        session_key: text(row, 3)?,
        chat_id: text(row, 4)?,
        thread_id: text(row, 5)?,
        origin_json: text(row, 6)?,
        model: text(row, 7)?,
        model_config: text(row, 8)?,
        parent_session_id: text(row, 9)?,
        started_at: real(row, 10)?.unwrap_or(0.0),
        ended_at: real(row, 11)?,
        end_reason: text(row, 12)?,
        input_tokens: int(row, 13)?,
        output_tokens: int(row, 14)?,
        cache_read_tokens: int(row, 15)?,
        cache_write_tokens: int(row, 16)?,
        reasoning_tokens: int(row, 17)?,
        cwd: text(row, 18)?,
        git_branch: text(row, 19)?,
        git_repo_root: text(row, 20)?,
        billing_provider: text(row, 21)?,
        billing_mode: text(row, 22)?,
        estimated_cost_usd: real(row, 23)?,
        actual_cost_usd: real(row, 24)?,
        title: text(row, 25)?,
        title_source: text(row, 26)?,
        last_activity_at: real(row, 27)?,
        archived: flag(row, 28)?,
        auto_archived: flag(row, 29)?,
        pinned: flag(row, 30)?,
        hidden: flag(row, 31)?,
    })
}

fn message_from_row(row: &Row<'_>) -> rusqlite::Result<HermesMessage> {
    Ok(HermesMessage {
        role: text(row, 0)?.unwrap_or_default(),
        content: text(row, 1)?,
        tool_call_id: text(row, 2)?,
        tool_calls: text(row, 3)?,
        tool_name: text(row, 4)?,
        timestamp: real(row, 5)?.unwrap_or(0.0),
        finish_reason: text(row, 6)?,
        reasoning: text(row, 7)?,
        reasoning_content: text(row, 8)?,
        reasoning_details: text(row, 9)?,
        codex_reasoning_items: text(row, 10)?,
        codex_message_items: text(row, 11)?,
        platform_message_id: text(row, 12)?,
        // A schema without the flags had every row active.
        active: row.get_ref(13)?.as_i64() != Ok(0),
        compacted: flag(row, 14)?,
    })
}

fn usage_from_row(row: &Row<'_>) -> rusqlite::Result<ModelUsage> {
    Ok(ModelUsage {
        model: text(row, 0)?.unwrap_or_default(),
        billing_provider: text(row, 1)?.unwrap_or_default(),
        billing_mode: text(row, 2)?.unwrap_or_default(),
        input_tokens: int(row, 3)?,
        output_tokens: int(row, 4)?,
        cache_read_tokens: int(row, 5)?,
        cache_write_tokens: int(row, 6)?,
        reasoning_tokens: int(row, 7)?,
        estimated_cost_usd: real(row, 8)?.unwrap_or(0.0),
        actual_cost_usd: real(row, 9)?.unwrap_or(0.0),
        last_seen: real(row, 10)?,
    })
}

/// `text` parsed as JSON, `None` for empty or invalid text.
#[must_use]
pub(super) fn json(text: Option<&str>) -> Option<Value> {
    text.filter(|t| !t.trim().is_empty())
        .and_then(|t| serde_json::from_str(t).ok())
}

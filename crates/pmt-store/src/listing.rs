//! Session listings, pickers, flags and size guards (spec: session-storage,
//! Integration source tag; Resume and export size guards; Session
//! management CLI).

use pmt_core::error::{Error, codes};
use pmt_core::id::SessionId;
use rusqlite::types::Value;
use rusqlite::{Connection, params, params_from_iter};

use crate::contention::WriteClass;
use crate::error::sql;
use crate::messages::{self, Message, Visibility};
use crate::model::{Archived, Source};
use crate::sessions::{self, COLUMNS, HIDDEN_SOURCE_TAGS, Session, now_ms, session_from_row};
use crate::store::Store;

/// Which sessions a listing returns, newest activity first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListOptions {
    /// At most this many.
    pub limit: Option<u32>,
    /// Also hidden, archived and integration-tagged (`oneshot`, `tool`)
    /// sessions.
    pub all: bool,
    /// Only these sources (empty: all).
    pub sources: Vec<Source>,
    /// Only pinned sessions.
    pub pinned_only: bool,
}

/// The sources `-c` and `--resume latest` pick from.
const INTERACTIVE_SOURCES: &[Source] = &[Source::Cli, Source::Tui];

fn hidden_tags_clause() -> String {
    let tags: Vec<String> = HIDDEN_SOURCE_TAGS
        .iter()
        .map(|t| format!("'{t}'"))
        .collect();
    format!(
        "(source_tag IS NULL OR source_tag NOT IN ({}))",
        tags.join(", ")
    )
}

fn query_sessions(
    conn: &Connection,
    filter: &str,
    params: &[Value],
    limit: Option<u32>,
) -> Result<Vec<Session>, Error> {
    let limit = limit.map_or(String::new(), |n| format!(" LIMIT {n}"));
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {COLUMNS} FROM sessions WHERE {filter} \
             ORDER BY last_activity_at DESC, id DESC{limit}"
        ))
        .map_err(sql)?;
    let mut rows = stmt.query(params_from_iter(params.iter())).map_err(sql)?;
    let mut found = Vec::new();
    while let Some(row) = rows.next().map_err(sql)? {
        found.push(session_from_row(row)?);
    }
    Ok(found)
}

fn source_list(sources: &[Source]) -> (String, Vec<Value>) {
    let marks = vec!["?"; sources.len()].join(", ");
    let values = sources
        .iter()
        .map(|s| Value::Text(s.as_str().to_owned()))
        .collect();
    (format!("source IN ({marks})"), values)
}

impl Store {
    /// Sessions by last activity, newest first.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn list_sessions(&self, options: &ListOptions) -> Result<Vec<Session>, Error> {
        let mut clauses = vec!["1 = 1".to_owned()];
        let mut params = Vec::new();
        if !options.all {
            clauses.push("hidden = 0 AND archived = 'none'".to_owned());
            clauses.push(hidden_tags_clause());
        }
        if !options.sources.is_empty() {
            let (clause, values) = source_list(&options.sources);
            clauses.push(clause);
            params.extend(values);
        }
        if options.pinned_only {
            clauses.push("pinned = 1".to_owned());
        }
        let filter = clauses.join(" AND ");
        self.read(|conn| query_sessions(conn, &filter, &params, options.limit))
    }

    /// The session `-c` resumes in `cwd`: the most recent CLI or TUI
    /// session started there, skipping hidden sessions and integration
    /// sessions (`oneshot`, `tool` tags).
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn continue_target(&self, cwd: &str) -> Result<Option<Session>, Error> {
        self.continue_target_from(cwd, None)
    }

    /// [`Store::continue_target`] preferring, when `terminal` is given,
    /// the newest such session last used from that terminal
    /// ([`Store::record_terminal`]); without one used there, the newest
    /// in `cwd`.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn continue_target_from(
        &self,
        cwd: &str,
        terminal: Option<&str>,
    ) -> Result<Option<Session>, Error> {
        let (sources, source_params) = source_list(INTERACTIVE_SOURCES);
        let mut params = vec![Value::Text(cwd.to_owned())];
        params.extend(source_params);
        let filter = format!(
            "cwd = ? AND hidden = 0 AND {} AND {sources}",
            hidden_tags_clause()
        );
        if let Some(terminal) = terminal {
            let mut mine = params.clone();
            mine.push(Value::Text(terminal.to_owned()));
            let filter = format!(
                "{filter} AND id IN (SELECT session_id FROM session_terminals WHERE terminal = ?)"
            );
            let found = self.read(|conn| query_sessions(conn, &filter, &mine, Some(1)))?;
            if let Some(session) = found.into_iter().next() {
                return Ok(Some(session));
            }
        }
        let found = self.read(|conn| query_sessions(conn, &filter, &params, Some(1)))?;
        Ok(found.into_iter().next())
    }

    /// Records that `session` was just used from `terminal` (an opaque
    /// identity of the controlling terminal; see
    /// [`Store::continue_target_from`]).
    ///
    /// # Errors
    ///
    /// `session.not_found`, `store.busy`, `store.sqlite`.
    pub fn record_terminal(&self, session: SessionId, terminal: &str) -> Result<(), Error> {
        self.write(|tx| {
            sessions::load(tx, session)?.ok_or_else(|| sessions::not_found(session))?;
            tx.execute(
                "INSERT INTO session_terminals (session_id, terminal, used_at) VALUES (?1, ?2, ?3) \
                 ON CONFLICT (session_id) DO UPDATE SET terminal = excluded.terminal, used_at = excluded.used_at",
                params![session.to_string(), terminal, now_ms()],
            )
            .map_err(sql)?;
            Ok(())
        })
    }

    /// Pins or unpins a session (pinned sessions are never pruned).
    ///
    /// # Errors
    ///
    /// `session.not_found`, `store.busy`, `store.sqlite`.
    pub fn set_pinned(&self, id: SessionId, pinned: bool) -> Result<Session, Error> {
        self.update_session(id, "pinned = ?2", &Value::Integer(i64::from(pinned)))
    }

    /// Sets the archive state (`user` to archive, `none` to unarchive).
    ///
    /// # Errors
    ///
    /// `session.not_found`, `store.busy`, `store.sqlite`.
    pub fn set_archived(&self, id: SessionId, archived: Archived) -> Result<Session, Error> {
        self.update_session(
            id,
            "archived = ?2",
            &Value::Text(archived.as_str().to_owned()),
        )
    }

    fn update_session(&self, id: SessionId, set: &str, value: &Value) -> Result<Session, Error> {
        self.write(|tx| {
            tx.execute(
                &format!("UPDATE sessions SET {set} WHERE id = ?1"),
                params![id.to_string(), value],
            )
            .map_err(sql)?;
            sessions::load(tx, id)?.ok_or_else(|| sessions::not_found(id))
        })
    }

    /// Activity heartbeat: bumps `last_activity_at` within the 0.5 s
    /// activity budget, so a busy store never stalls a turn.
    ///
    /// # Errors
    ///
    /// `store.busy` (after about 0.5 s), `session.not_found`,
    /// `store.sqlite`.
    pub fn record_activity(&self, id: SessionId) -> Result<(), Error> {
        self.write_as(WriteClass::Activity, |tx| {
            let changed = tx
                .execute(
                    "UPDATE sessions SET last_activity_at = ?2 WHERE id = ?1",
                    params![id.to_string(), now_ms()],
                )
                .map_err(sql)?;
            if changed == 0 {
                return Err(sessions::not_found(id));
            }
            Ok(())
        })
    }

    /// Active messages over `id`'s compression lineage.
    ///
    /// # Errors
    ///
    /// `session.not_found`, `store.sqlite`.
    pub fn active_lineage_messages(&self, id: SessionId) -> Result<u64, Error> {
        let lineage = self.lineage(id)?;
        self.read(|conn| {
            let mut stmt = conn
                .prepare_cached(
                    "SELECT count(*) FROM messages WHERE session_id = ?1 AND active = 1",
                )
                .map_err(sql)?;
            let mut total = 0;
            for session in &lineage {
                let n: i64 = stmt
                    .query_row([session.to_string()], |row| row.get(0))
                    .map_err(sql)?;
                total += u64::try_from(n).unwrap_or(0);
            }
            Ok(total)
        })
    }

    /// Refuses to resume a lineage with more than `max` active messages
    /// (`0` disables the guard), before any transcript is loaded.
    ///
    /// # Errors
    ///
    /// `session.too_large_to_resume`, `session.not_found`, `store.sqlite`.
    pub fn check_resumable(&self, id: SessionId, max: u64) -> Result<(), Error> {
        if max == 0 {
            return Ok(());
        }
        let active = self.active_lineage_messages(id)?;
        if active <= max {
            return Ok(());
        }
        Err(Error::new(
            &codes::SESSION_TOO_LARGE_TO_RESUME,
            format!(
                "session {id} has {active} active messages in its lineage, more than sessions.max_resume_messages ({max})"
            ),
        )
        .with_hint("compact it with /compress, or fork it from a recent message"))
    }

    /// The transcript for an in-memory export, refused above `max` rows.
    ///
    /// # Errors
    ///
    /// `session.too_large_to_export`, blob and SQLite errors.
    pub fn messages_for_export(
        &self,
        id: SessionId,
        visibility: Visibility,
        max: u64,
    ) -> Result<Vec<Message>, Error> {
        let rows: i64 = self.read(|conn| {
            let filter = if visibility == Visibility::Active {
                " AND active = 1"
            } else {
                ""
            };
            conn.query_row(
                &format!("SELECT count(*) FROM messages WHERE session_id = ?1{filter}"),
                [id.to_string()],
                |row| row.get(0),
            )
            .map_err(sql)
        })?;
        let rows = u64::try_from(rows).unwrap_or(0);
        if rows > max {
            return Err(Error::new(
                &codes::SESSION_TOO_LARGE_TO_EXPORT,
                format!(
                    "session {id} has {rows} messages, more than sessions.max_export_messages ({max}) for an in-memory export"
                ),
            )
            .with_hint("export it to a file: prometheus sessions export <ref> -o FILE"));
        }
        self.messages(id, visibility)
    }

    /// Streams a transcript row by row (exports of any size).
    ///
    /// # Errors
    ///
    /// Blob and SQLite errors, or the first error `each` returns.
    pub fn for_each_message(
        &self,
        id: SessionId,
        visibility: Visibility,
        each: impl FnMut(Message) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let blob_dir = self.location().blobs().to_path_buf();
        self.read(|conn| messages::for_each(conn, &blob_dir, id, visibility, each))
    }
}

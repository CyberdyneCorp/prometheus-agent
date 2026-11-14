//! `board.db`: one SQLite database per board (WAL, mode 0600). Every
//! mutation runs in one `BEGIN IMMEDIATE` transaction, so two processes
//! racing for the same task (two claims, an archive and a completion)
//! serialise and exactly one wins.

use std::path::Path;
use std::time::Duration;

use pmt_core::error::{Error, codes};
use rusqlite::Connection;

/// The schema version this build writes.
pub(crate) const SCHEMA_VERSION: i64 = 1;

/// How long a writer waits for another one.
const BUSY_TIMEOUT: Duration = Duration::from_secs(15);

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS tasks (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    body TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL,
    assignee TEXT,
    reviewer TEXT,
    priority INTEGER NOT NULL DEFAULT 0,
    workspace TEXT NOT NULL,
    workspace_path TEXT,
    kind TEXT NOT NULL,
    review INTEGER,
    idempotency_key TEXT,
    block_kind TEXT,
    block_reason TEXT,
    blocked_from TEXT,
    block_history TEXT NOT NULL DEFAULT '[]',
    wake_at INTEGER,
    not_before INTEGER,
    consecutive_failures INTEGER NOT NULL DEFAULT 0,
    last_failure TEXT,
    max_retries INTEGER,
    breaker_tripped INTEGER NOT NULL DEFAULT 0,
    budget_usd REAL,
    budget_tokens INTEGER,
    artifacts TEXT NOT NULL DEFAULT '[]',
    result TEXT,
    summary TEXT,
    created_by TEXT NOT NULL,
    created_run TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    completed_at INTEGER
);
CREATE INDEX IF NOT EXISTS tasks_status ON tasks(status, priority, created_at);
CREATE INDEX IF NOT EXISTS tasks_idempotency ON tasks(idempotency_key) WHERE idempotency_key IS NOT NULL;
CREATE TABLE IF NOT EXISTS links (
    parent TEXT NOT NULL REFERENCES tasks(id),
    child TEXT NOT NULL REFERENCES tasks(id),
    PRIMARY KEY (parent, child)
);
CREATE INDEX IF NOT EXISTS links_child ON links(child);
CREATE TABLE IF NOT EXISTS events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id TEXT,
    run_id TEXT,
    actor TEXT NOT NULL,
    kind TEXT NOT NULL,
    payload TEXT NOT NULL DEFAULT '{}',
    at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS events_task ON events(task_id, id);
CREATE TRIGGER IF NOT EXISTS events_immutable BEFORE UPDATE ON events
BEGIN SELECT RAISE(ABORT, 'kanban events are immutable'); END;
CREATE TABLE IF NOT EXISTS task_runs (
    id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL REFERENCES tasks(id),
    profile TEXT NOT NULL,
    role TEXT NOT NULL,
    started_at INTEGER NOT NULL,
    ended_at INTEGER,
    outcome TEXT,
    exit_code INTEGER,
    worker_exit INTEGER,
    session_id TEXT,
    trajectory_id TEXT,
    cost_usd REAL NOT NULL DEFAULT 0,
    tokens INTEGER NOT NULL DEFAULT 0,
    pid INTEGER,
    pid_start INTEGER,
    lease_expires_at INTEGER,
    heartbeat_at INTEGER,
    ticket_hash TEXT,
    ticket_used INTEGER NOT NULL DEFAULT 0,
    worker_gone INTEGER NOT NULL DEFAULT 0,
    error TEXT
);
CREATE INDEX IF NOT EXISTS runs_task ON task_runs(task_id, started_at);
CREATE INDEX IF NOT EXISTS runs_open ON task_runs(ended_at) WHERE ended_at IS NULL;
CREATE TABLE IF NOT EXISTS comments (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id TEXT NOT NULL REFERENCES tasks(id),
    author TEXT NOT NULL,
    body TEXT NOT NULL,
    at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS attachments (
    id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL REFERENCES tasks(id),
    filename TEXT NOT NULL,
    content_type TEXT NOT NULL,
    size INTEGER NOT NULL,
    sha256 TEXT NOT NULL,
    uploader TEXT NOT NULL,
    at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS subscriptions (
    id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL REFERENCES tasks(id),
    adapter TEXT NOT NULL,
    chat TEXT NOT NULL,
    thread TEXT,
    chat_type TEXT,
    delivery_mode TEXT NOT NULL DEFAULT 'notify',
    profile TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    checkpoint INTEGER NOT NULL DEFAULT 0,
    warned INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS dispatcher_lease (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    holder TEXT NOT NULL,
    pid INTEGER NOT NULL,
    pid_start INTEGER NOT NULL,
    host TEXT NOT NULL,
    expires_at INTEGER NOT NULL,
    acquired_at INTEGER NOT NULL
);
";

/// Maps a SQLite error.
#[allow(clippy::needless_pass_by_value)] // the shape `map_err(sql)` needs
pub(crate) fn sql(err: rusqlite::Error) -> Error {
    Error::new(&codes::HOME_IO, format!("board database: {err}"))
}

/// Opens (and creates) `path`.
///
/// # Errors
///
/// `home.io`.
pub(crate) fn open(path: &Path) -> Result<Connection, Error> {
    let fresh = !path.exists();
    let conn = Connection::open(path).map_err(sql)?;
    if fresh {
        tighten(path);
    }
    conn.busy_timeout(BUSY_TIMEOUT).map_err(sql)?;
    conn.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))
        .map_err(sql)?;
    conn.execute_batch("PRAGMA foreign_keys = ON; PRAGMA synchronous = NORMAL;")
        .map_err(sql)?;
    conn.execute_batch(SCHEMA).map_err(sql)?;
    conn.execute(
        "INSERT OR IGNORE INTO meta(key, value) VALUES ('schema_version', ?1)",
        [SCHEMA_VERSION.to_string()],
    )
    .map_err(sql)?;
    Ok(conn)
}

#[cfg(unix)]
fn tighten(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn tighten(_: &Path) {}

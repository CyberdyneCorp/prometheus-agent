//! A fixture Hermes home for the migration tests: profile files written
//! as Hermes writes them and a `state.db` on the real Hermes schema
//! (v32, `fixtures/hermes-migrate/state-schema-v32.sql`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, params};

/// The Hermes schema the fixture databases use.
pub const SCHEMA: &str = include_str!("../fixtures/hermes-migrate/state-schema-v32.sql");

/// A Hermes home under a test directory.
pub struct Hermes {
    /// `~/.hermes`.
    pub root: PathBuf,
}

/// One Hermes session row (the fields the tests vary).
#[derive(Debug, Clone, Default)]
pub struct Session<'a> {
    pub id: &'a str,
    pub source: &'a str,
    pub parent: Option<&'a str>,
    pub end_reason: Option<&'a str>,
    pub started_at: f64,
    pub title: Option<&'a str>,
    pub model: Option<&'a str>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub model_config: Option<&'a str>,
}

impl Hermes {
    /// A new Hermes home at `root`.
    pub fn new(root: &Path) -> Self {
        std::fs::create_dir_all(root).unwrap();
        Self {
            root: root.to_owned(),
        }
    }

    /// A profile's directory (`default` is the root).
    pub fn dir(&self, profile: &str) -> PathBuf {
        if profile == "default" {
            self.root.clone()
        } else {
            self.root.join("profiles").join(profile)
        }
    }

    /// Writes `text` to `relative` of `profile`.
    pub fn write(&self, profile: &str, relative: &str, text: &str) -> PathBuf {
        let path = self.dir(profile).join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        path
    }

    /// The profile's `state.db`, created on the Hermes schema at
    /// `version` when missing.
    pub fn db(&self, profile: &str, version: i64) -> Connection {
        let path = self.dir(profile).join("state.db");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let fresh = !path.exists();
        let conn = Connection::open(&path).unwrap();
        if fresh {
            conn.pragma_update(None, "journal_mode", "wal").unwrap();
            conn.execute_batch(SCHEMA).unwrap();
            conn.execute(
                "INSERT INTO schema_version (version) VALUES (?1)",
                [version],
            )
            .unwrap();
        }
        conn
    }
}

/// Inserts a session row.
pub fn add_session(conn: &Connection, s: &Session<'_>) {
    let ended = s.end_reason.map(|_| s.started_at + 60.0);
    conn.execute(
        "INSERT INTO sessions (id, source, parent_session_id, end_reason, started_at, ended_at, \
         title, title_source, model, input_tokens, output_tokens, model_config, \
         last_activity_at, billing_provider) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'user', ?8, ?9, ?10, ?11, ?5, 'anthropic')",
        params![
            s.id,
            if s.source.is_empty() { "cli" } else { s.source },
            s.parent,
            s.end_reason,
            s.started_at,
            ended,
            s.title,
            s.model,
            s.input_tokens,
            s.output_tokens,
            s.model_config,
        ],
    )
    .unwrap();
}

/// Appends a message to `session`.
pub fn add_message(conn: &Connection, session: &str, role: &str, content: &str, ts: f64) {
    conn.execute(
        "INSERT INTO messages (session_id, role, content, timestamp) VALUES (?1, ?2, ?3, ?4)",
        params![session, role, content, ts],
    )
    .unwrap();
}

/// Every file under `root` with its content, size, mode and modification
/// time: what "unchanged" means for the read-only source guarantee.
pub fn attributes(root: &Path) -> BTreeMap<PathBuf, (Vec<u8>, u64, u32, std::time::SystemTime)> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_owned()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            let meta = std::fs::symlink_metadata(&path).unwrap();
            if meta.is_dir() {
                stack.push(path.clone());
            }
            #[cfg(unix)]
            let mode = std::os::unix::fs::PermissionsExt::mode(&meta.permissions());
            #[cfg(not(unix))]
            let mode = u32::from(meta.permissions().readonly());
            let bytes = if meta.is_file() {
                std::fs::read(&path).unwrap()
            } else {
                Vec::new()
            };
            out.insert(path, (bytes, meta.len(), mode, meta.modified().unwrap()));
        }
    }
    out
}

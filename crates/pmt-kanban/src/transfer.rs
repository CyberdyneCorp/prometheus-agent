//! Board export and import (spec: kanban, Board export and import): a
//! versioned JSON archive of a board's tasks, links, events, runs,
//! comments and attachments (blob bytes inline), restored as a new board.
//! Task ids are kept unless another board of the home already uses one,
//! in which case that task gets a fresh id everywhere it is referenced.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use pmt_core::error::{Error, codes};
use pmt_core::private_fs;
use rusqlite::types::ValueRef;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::attach;
use crate::board::create::task_id;
use crate::boards::{BoardSettings, Boards};
use crate::db::sql;

/// The archive format this build writes and reads.
pub const FORMAT: &str = "prometheus.kanban-board.v1";

/// The tables an archive carries, in insertion order.
const TABLES: [&str; 6] = [
    "tasks",
    "links",
    "comments",
    "attachments",
    "task_runs",
    "events",
];

/// Columns holding task ids, per table.
const TASK_REFS: [(&str, &[&str]); 6] = [
    ("tasks", &["id"]),
    ("links", &["parent", "child"]),
    ("comments", &["task_id"]),
    ("attachments", &["task_id"]),
    ("task_runs", &["task_id"]),
    ("events", &["task_id"]),
];

/// One board archive.
#[derive(Debug, Serialize, Deserialize)]
pub struct Archive {
    /// [`FORMAT`].
    pub format: String,
    /// The exported board's slug.
    pub board: String,
    /// Its `board.toml`.
    pub settings: BoardSettings,
    /// Rows per table, as column → value objects.
    pub tables: BTreeMap<String, Vec<Map<String, Value>>>,
    /// Attachment bytes by SHA-256, base64.
    pub blobs: BTreeMap<String, String>,
}

/// What an import did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Imported {
    /// The new board.
    pub board: String,
    /// Tasks restored.
    pub tasks: usize,
    /// Task ids that collided and were remapped (old → new).
    pub remapped: BTreeMap<String, String>,
}

fn invalid(message: impl Into<String>) -> Error {
    Error::new(&codes::KANBAN_INVALID_ARCHIVE, message)
}

fn to_json(value: ValueRef<'_>) -> Value {
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => Value::from(i),
        ValueRef::Real(f) => Value::from(f),
        ValueRef::Text(t) => Value::from(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Value::from(STANDARD.encode(b)),
    }
}

fn dump(conn: &rusqlite::Connection, table: &str) -> Result<Vec<Map<String, Value>>, Error> {
    let mut stmt = conn
        .prepare(&format!("SELECT * FROM {table}"))
        .map_err(sql)?;
    let names: Vec<String> = stmt.column_names().map(str::to_owned).collect();
    let rows = stmt
        .query_map([], |row| {
            let mut map = Map::new();
            for (i, name) in names.iter().enumerate() {
                map.insert(name.clone(), to_json(row.get_ref(i)?));
            }
            Ok(map)
        })
        .map_err(sql)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql)?;
    Ok(rows)
}

/// Writes board `slug` to `file`.
///
/// # Errors
///
/// `kanban.board_not_found`, `home.io`.
pub fn export(boards: &Boards, slug: &str, file: &Path) -> Result<usize, Error> {
    let board = boards.board_any(slug)?;
    let mut tables = BTreeMap::new();
    for table in TABLES {
        tables.insert(table.to_owned(), dump(board.conn(), table)?);
    }
    let mut blobs = BTreeMap::new();
    for row in &tables["attachments"] {
        if let Some(sha) = row.get("sha256").and_then(Value::as_str) {
            let path = attach::blob_path(board.blobs(), sha)
                .ok_or_else(|| invalid(format!("attachment hash '{sha}' is malformed")))?;
            let bytes = std::fs::read(&path).map_err(|e| Error::io(&codes::HOME_IO, &path, &e))?;
            blobs.insert(sha.to_owned(), STANDARD.encode(bytes));
        }
    }
    let count = tables["tasks"].len();
    let archive = Archive {
        format: FORMAT.to_owned(),
        board: slug.to_owned(),
        settings: board.settings().clone(),
        tables,
        blobs,
    };
    let text = serde_json::to_vec_pretty(&archive).unwrap_or_default();
    private_fs::write_atomic(file, &text).map_err(|e| Error::io(&codes::HOME_IO, file, &e))?;
    Ok(count)
}

/// Task ids used by every board of the home.
fn home_ids(boards: &Boards) -> Result<HashSet<String>, Error> {
    let mut ids = HashSet::new();
    for info in boards.list()? {
        let board = boards.board_any(&info.slug)?;
        for task in board.tasks(&crate::board::Filter {
            archived: true,
            ..Default::default()
        })? {
            ids.insert(task.id);
        }
    }
    Ok(ids)
}

/// Fresh ids for the archive's task ids that `taken` already holds.
fn fresh_ids(archive: &Archive, taken: &HashSet<String>) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let mut used = taken.clone();
    let ids = archive
        .tables
        .get("tasks")
        .into_iter()
        .flatten()
        .filter_map(|row| row.get("id").and_then(Value::as_str));
    for id in ids.filter(|id| taken.contains(*id)) {
        let mut fresh = task_id();
        while used.contains(&fresh) {
            fresh = task_id();
        }
        used.insert(fresh.clone());
        map.insert(id.to_owned(), fresh);
    }
    map
}

/// Rewrites every task-id column of the archive through `map`.
fn apply(archive: &mut Archive, map: &HashMap<String, String>) {
    for (table, columns) in TASK_REFS {
        for row in archive.tables.get_mut(table).into_iter().flatten() {
            for column in columns {
                let new = row
                    .get(*column)
                    .and_then(Value::as_str)
                    .and_then(|id| map.get(id))
                    .cloned();
                if let Some(new) = new {
                    row.insert((*column).to_owned(), Value::from(new));
                }
            }
        }
    }
}

fn remap(archive: &mut Archive, taken: &HashSet<String>) -> BTreeMap<String, String> {
    let map = fresh_ids(archive, taken);
    apply(archive, &map);
    map.into_iter().collect()
}

fn insert(
    conn: &rusqlite::Connection,
    table: &str,
    rows: &[Map<String, Value>],
) -> Result<(), Error> {
    let known: HashSet<String> = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .and_then(|mut s| s.query_map([], |r| r.get::<_, String>(1))?.collect())
        .map_err(sql)?;
    for row in rows {
        let columns: Vec<&String> = row.keys().filter(|k| known.contains(*k)).collect();
        let marks: Vec<String> = (1..=columns.len()).map(|i| format!("?{i}")).collect();
        let query = format!(
            "INSERT INTO {table}({}) VALUES ({})",
            columns
                .iter()
                .map(|c| c.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            marks.join(", ")
        );
        let values: Vec<rusqlite::types::Value> = columns
            .iter()
            .map(|c| match &row[*c] {
                Value::Null => rusqlite::types::Value::Null,
                Value::Bool(b) => rusqlite::types::Value::Integer(i64::from(*b)),
                Value::Number(n) => n.as_i64().map_or_else(
                    || rusqlite::types::Value::Real(n.as_f64().unwrap_or(0.0)),
                    rusqlite::types::Value::Integer,
                ),
                Value::String(s) => rusqlite::types::Value::Text(s.clone()),
                other => rusqlite::types::Value::Text(other.to_string()),
            })
            .collect();
        conn.execute(&query, rusqlite::params_from_iter(values))
            .map_err(|e| invalid(format!("{table}: {e}")))?;
    }
    Ok(())
}

/// Refuses an archive unless every blob key and every attachment row's
/// `sha256` is a well-formed hash, and every row's blob is in the archive
/// or already in the store: the hash becomes a path under the blob store,
/// so an unchecked one could point workers and exports anywhere.
fn check_attachments(archive: &Archive, blobs: &Path) -> Result<(), Error> {
    if let Some(sha) = archive.blobs.keys().find(|sha| !attach::is_sha256(sha)) {
        return Err(invalid(format!("attachment hash '{sha}' is malformed")));
    }
    let rows = archive
        .tables
        .get("attachments")
        .map_or(&[][..], Vec::as_slice);
    for row in rows {
        let sha = row
            .get("sha256")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let Some(path) = attach::blob_path(blobs, sha) else {
            return Err(invalid(format!("attachment hash '{sha}' is malformed")));
        };
        if !archive.blobs.contains_key(sha) && !path.is_file() {
            return Err(invalid(format!(
                "attachment {sha} has no data in the archive"
            )));
        }
    }
    Ok(())
}

/// Restores the archive in `file` as a new board named `rename`, else
/// the archived slug.
///
/// # Errors
///
/// `kanban.invalid_archive`, `kanban.board_exists` (pass `--rename`),
/// slug and store errors.
pub fn import(boards: &Boards, file: &Path, rename: Option<&str>) -> Result<Imported, Error> {
    let bytes = std::fs::read(file).map_err(|e| Error::io(&codes::HOME_IO, file, &e))?;
    let mut archive: Archive = serde_json::from_slice(&bytes)
        .map_err(|_| invalid(format!("{} is not a board archive", file.display())))?;
    if archive.format != FORMAT {
        return Err(invalid(format!(
            "unsupported board archive format '{}'",
            archive.format
        )));
    }
    let wanted = crate::boards::slug(rename.unwrap_or(&archive.board))?;
    if boards.dir(&wanted).join("board.toml").exists() {
        return Err(Error::new(
            &codes::KANBAN_BOARD_EXISTS,
            format!("board '{wanted}' already exists; pass --rename <new>"),
        ));
    }
    check_attachments(&archive, &boards.blobs_dir())?;
    for (sha, data) in &archive.blobs {
        let bytes = STANDARD
            .decode(data)
            .map_err(|_| invalid("attachment data is not base64"))?;
        if attach::put_blob(&boards.blobs_dir(), &bytes)? != *sha {
            return Err(invalid(format!("attachment {sha} does not match its hash")));
        }
    }
    let remapped = remap(&mut archive, &home_ids(boards)?);
    let slug = boards.create(&wanted, Some(&archive.settings.name))?;
    let mut settings = archive.settings.clone();
    settings.archived = false;
    boards.save_settings(&slug, &settings)?;
    let mut board = boards.board(&slug)?;
    board.write(|tx| {
        for table in TABLES {
            insert(tx, table, archive.tables.get(table).map_or(&[][..], Vec::as_slice))?;
        }
        // Runs are history now: no imported run is open or claimable.
        tx.execute("UPDATE task_runs SET ended_at = COALESCE(ended_at, started_at), outcome = COALESCE(outcome, 'reclaimed'), ticket_used = 1", [])
            .map_err(sql)?;
        tx.execute("UPDATE tasks SET status = 'ready' WHERE status = 'running'", [])
            .map_err(sql)?;
        Ok(())
    })?;
    Ok(Imported {
        board: slug,
        tasks: archive.tables.get("tasks").map_or(0, Vec::len),
        remapped,
    })
}

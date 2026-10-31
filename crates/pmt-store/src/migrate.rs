//! Embedded, ordered, forward-only schema migrations (spec: session-storage,
//! Embedded versioned migrations).
//!
//! `store_meta` holds `schema_version` (what the store is at) and
//! `min_read_version` (the oldest build schema that may still read it, so a
//! future incompatible layout can lock old binaries out entirely). Each step
//! runs in its own `BEGIN IMMEDIATE` transaction that re-reads the version,
//! so two processes upgrading at once apply every step exactly once.

use std::path::{Path, PathBuf};

use pmt_core::private_fs;

use pmt_core::error::Error;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

use crate::error::{self, sql};
use crate::files;

/// One schema step.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Migration {
    /// The version the store is at after this step.
    pub version: u32,
    /// The oldest build schema that can read the result.
    pub min_read_version: u32,
    /// The DDL/DML of the step.
    pub sql: &'static str,
    /// The step rebuilds a table other tables reference: it runs with
    /// foreign keys off and `foreign_key_check` must pass before commit.
    pub rebuilds_tables: bool,
}

/// Every migration, in order; the last one's version is
/// [`crate::SCHEMA_VERSION`].
pub(crate) const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        min_read_version: 1,
        sql: include_str!("schema/v1.sql"),
        rebuilds_tables: false,
    },
    Migration {
        version: 2,
        min_read_version: 2,
        sql: include_str!("schema/v2.sql"),
        rebuilds_tables: true,
    },
    Migration {
        version: 3,
        min_read_version: 2,
        sql: include_str!("schema/v3.sql"),
        rebuilds_tables: false,
    },
    Migration {
        version: 4,
        min_read_version: 2,
        sql: include_str!("schema/v4.sql"),
        rebuilds_tables: false,
    },
    Migration {
        version: 5,
        min_read_version: 2,
        sql: include_str!("schema/v5.sql"),
        rebuilds_tables: false,
    },
    Migration {
        version: 6,
        min_read_version: 2,
        sql: include_str!("schema/v6.sql"),
        rebuilds_tables: false,
    },
    Migration {
        version: 7,
        min_read_version: 2,
        sql: include_str!("schema/v7.sql"),
        rebuilds_tables: false,
    },
    Migration {
        version: 8,
        min_read_version: 2,
        sql: include_str!("schema/v8.sql"),
        rebuilds_tables: false,
    },
    Migration {
        version: 9,
        min_read_version: 2,
        sql: include_str!("schema/v9.sql"),
        rebuilds_tables: false,
    },
    Migration {
        version: 10,
        min_read_version: 2,
        sql: include_str!("schema/v10.sql"),
        rebuilds_tables: false,
    },
];

/// Pre-migration copies kept in `<profile>/backups/state/`.
pub(crate) const BACKUPS_KEPT: usize = 3;

/// The identity a fresh store records.
pub(crate) struct Identity<'a> {
    pub profile_id: &'a str,
    pub profile_name: &'a str,
}

/// What `store_meta` says about an existing store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Meta {
    pub profile_id: Option<String>,
    pub profile_name: Option<String>,
    pub schema_version: u32,
    pub min_read_version: u32,
}

/// Reads `store_meta`; `None` for an empty database (a store being
/// created). A non-empty database without `store_meta` is reported with no
/// profile id, which never matches the opening profile.
pub(crate) fn read_meta(conn: &Connection) -> Result<Option<Meta>, Error> {
    let has_meta: bool = conn
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE name = 'store_meta')",
            [],
            |row| row.get(0),
        )
        .map_err(sql)?;
    if !has_meta {
        let objects: i64 = conn
            .query_row("SELECT count(*) FROM sqlite_master", [], |row| row.get(0))
            .map_err(sql)?;
        return Ok((objects > 0).then_some(Meta {
            profile_id: None,
            profile_name: None,
            schema_version: 0,
            min_read_version: 0,
        }));
    }
    let get = |key: &str| -> Result<Option<String>, Error> {
        conn.query_row(
            "SELECT value FROM store_meta WHERE key = ?1",
            [key],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql)
    };
    let number = |key: &str| -> Result<u32, Error> {
        Ok(get(key)?.and_then(|v| v.parse().ok()).unwrap_or(0))
    };
    Ok(Some(Meta {
        profile_id: get("profile_id")?,
        profile_name: get("profile_name")?,
        schema_version: number("schema_version")?,
        min_read_version: number("min_read_version")?,
    }))
}

/// Brings the store up to the last of `migrations`, copying an existing
/// store to `backup_dir` first. Returns the version the store was at.
pub(crate) fn run(
    conn: &mut Connection,
    migrations: &[Migration],
    identity: &Identity<'_>,
    backup_dir: &Path,
) -> Result<u32, Error> {
    let target = migrations.last().map_or(0, |m| m.version);
    let start = read_meta(conn)?.map_or(0, |meta| meta.schema_version);
    if start >= target {
        return Ok(start);
    }
    if start > 0 {
        backup(conn, backup_dir, start)?;
    }
    for step in migrations {
        apply(conn, step, identity)?;
    }
    Ok(start)
}

fn apply(conn: &mut Connection, step: &Migration, identity: &Identity<'_>) -> Result<(), Error> {
    if !step.rebuilds_tables {
        return apply_in_tx(conn, step, identity);
    }
    // `foreign_keys` cannot change inside a transaction.
    conn.pragma_update(None, "foreign_keys", false)
        .map_err(sql)?;
    let applied = apply_in_tx(conn, step, identity);
    conn.pragma_update(None, "foreign_keys", true)
        .map_err(sql)?;
    applied
}

fn apply_in_tx(
    conn: &mut Connection,
    step: &Migration,
    identity: &Identity<'_>,
) -> Result<(), Error> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql)?;
    let current = read_meta(&tx)?.map_or(0, |meta| meta.schema_version);
    if current >= step.version {
        return Ok(());
    }
    tx.execute_batch(step.sql).map_err(sql)?;
    if step.rebuilds_tables {
        check_foreign_keys(&tx)?;
    }
    let set = |key: &str, value: &str| {
        tx.execute(
            "INSERT INTO store_meta (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            [key, value],
        )
        .map_err(sql)
    };
    if current == 0 {
        set("profile_id", identity.profile_id)?;
        set("profile_name", identity.profile_name)?;
        set("store_uuid", &ulid::Ulid::generate().to_string())?;
        set("created_at", &jiff::Timestamp::now().to_string())?;
    }
    set("schema_version", &step.version.to_string())?;
    set("min_read_version", &step.min_read_version.to_string())?;
    tx.commit().map_err(sql)
}

fn check_foreign_keys(conn: &Connection) -> Result<(), Error> {
    let violation: Option<String> = conn
        .query_row("PRAGMA foreign_key_check", [], |row| row.get(0))
        .optional()
        .map_err(sql)?;
    match violation {
        None => Ok(()),
        Some(table) => Err(error::corrupt(
            "foreign keys",
            format!("table {table} references a missing row"),
        )),
    }
}

/// `VACUUM INTO <backup_dir>/state-v<old>-<timestamp>.db` (a consistent
/// copy of the live database), keeping the newest [`BACKUPS_KEPT`].
fn backup(conn: &Connection, backup_dir: &Path, old: u32) -> Result<(), Error> {
    files::private_dir(backup_dir)?;
    let target = new_backup_file(backup_dir, old)?;
    let path = target.to_str().ok_or_else(|| {
        error::io(
            &target,
            &std::io::Error::new(std::io::ErrorKind::InvalidInput, "path is not UTF-8"),
        )
    })?;
    conn.execute("VACUUM INTO ?1", [path]).map_err(sql)?;
    prune_backups(backup_dir)
}

/// Creates an empty 0600 backup file (VACUUM INTO accepts an empty file
/// and keeps its mode), never reusing an existing name.
fn new_backup_file(dir: &Path, old: u32) -> Result<PathBuf, Error> {
    let stamp = jiff::Timestamp::now().strftime("%Y%m%dT%H%M%S%.3fZ");
    for n in 1.. {
        let suffix = if n == 1 {
            String::new()
        } else {
            format!("-{n}")
        };
        let target = dir.join(format!("state-v{old}-{stamp}{suffix}.db"));
        match private_fs::new_file_options().open(&target) {
            Ok(_) => return Ok(target),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(error::io(&target, &e)),
        }
    }
    unreachable!("the loop returns")
}

fn prune_backups(dir: &Path) -> Result<(), Error> {
    let entries = std::fs::read_dir(dir).map_err(|e| error::io(dir, &e))?;
    let mut backups: Vec<(String, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let stamp = name.strip_prefix("state-v")?.split_once('-')?.1.to_owned();
            let is_db = std::path::Path::new(&name)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("db"));
            is_db.then(|| (stamp, entry.path()))
        })
        .collect();
    // Timestamps sort chronologically; newest first.
    backups.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, path) in backups.into_iter().skip(BACKUPS_KEPT) {
        std::fs::remove_file(&path).map_err(|e| error::io(&path, &e))?;
    }
    Ok(())
}

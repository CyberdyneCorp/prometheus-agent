//! Opening a profile's store and running transactions on it (spec:
//! session-storage, Store opened only through `ProfileScope`; Owner-only
//! file permissions; Embedded versioned migrations; WAL mode, bounded
//! readers and checkpoints).
//!
//! One [`Store`] holds a single writer connection (writes are serialized
//! in-process; across processes SQLite's `BEGIN IMMEDIATE` lock does it)
//! and a pool of at most [`MAX_READERS`] read-only connections. In WAL mode
//! readers never block the writer.

use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use pmt_config::schema::JournalMode;
use pmt_config::{ProfileDir, ProfileScope};
use pmt_core::error::{Error, codes};
use pmt_core::id::ProfileId;
use rusqlite::{Connection, OpenFlags, Transaction, TransactionBehavior};

use crate::error::sql;
use crate::files;
use crate::location::StoreLocation;
use crate::migrate::{self, Identity, Meta, Migration};

/// Read-only connections per store.
pub const MAX_READERS: usize = 8;

/// Committed write transactions between passive checkpoints.
pub const CHECKPOINT_EVERY: u32 = 1000;

/// How long one statement waits for another process's lock. The retry
/// policy around it (time budgets per write class) is the write-contention
/// requirement's.
const BUSY_TIMEOUT: Duration = Duration::from_secs(1);

/// A profile's open session store.
pub struct Store {
    location: StoreLocation,
    profile: ProfileDir,
    profile_id: ProfileId,
    /// `Some(found)` when the file's schema is newer than this build: the
    /// store is then read-only.
    newer_schema: Option<u32>,
    supported: u32,
    writer: Mutex<Writer>,
    readers: ReaderPool,
}

struct Writer {
    conn: Connection,
    writes_since_checkpoint: u32,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("database", &self.location.database())
            .field("profile_id", &self.profile_id)
            .field("newer_schema", &self.newer_schema)
            .finish_non_exhaustive()
    }
}

impl Store {
    /// Opens (creating and migrating as needed) the store of the profile
    /// `scope` was loaded for.
    ///
    /// # Errors
    ///
    /// `profile.deleted`, `profile.id_invalid`, `store.profile_mismatch`
    /// (the file belongs to another profile; nothing was written),
    /// `store.schema_too_new` (the newer build made the store unreadable to
    /// this one), `store.unsupported_filesystem`, `store.io`,
    /// `store.sqlite`.
    pub fn open(scope: &ProfileScope) -> Result<Self, Error> {
        Self::open_with(scope, migrate::MIGRATIONS)
    }

    pub(crate) fn open_with(scope: &ProfileScope, migrations: &[Migration]) -> Result<Self, Error> {
        let profile = scope.profile().clone();
        profile.ensure_live()?;
        let profile_id = profile.id()?;
        let location = StoreLocation::of(scope);
        let database = location.database().to_path_buf();
        files::private_dir(location.blobs())?;
        let journal = scope.config().storage.journal_mode;
        if journal == JournalMode::Wal {
            files::check_wal_supported(scope.dir())?;
        }
        files::create_private(&database)?;
        for path in sidecars(&database) {
            files::tighten(&path, 0o600);
        }
        let mut conn = Connection::open(&database).map_err(sql)?;
        conn.busy_timeout(BUSY_TIMEOUT).map_err(sql)?;
        conn.pragma_update(None, "foreign_keys", true)
            .map_err(sql)?;
        let supported = migrations.last().map_or(0, |m| m.version);
        let meta = migrate::read_meta(&conn)?;
        if let Some(meta) = &meta {
            check_identity(meta, &profile_id, &profile, &database)?;
            check_readable(meta, supported)?;
        }
        let newer_schema = meta
            .as_ref()
            .map(|m| m.schema_version)
            .filter(|found| *found > supported);
        if newer_schema.is_some() {
            conn.pragma_update(None, "query_only", true).map_err(sql)?;
        } else {
            set_journal_mode(&conn, journal)?;
            // Checkpoints follow CHECKPOINT_EVERY, not page counts.
            conn.pragma_update(None, "wal_autocheckpoint", 0)
                .map_err(sql)?;
            let identity = Identity {
                profile_id: &profile_id.to_string(),
                profile_name: profile.name().as_str(),
            };
            let backups = profile.path().join("backups").join("state");
            migrate::run(&mut conn, migrations, &identity, &backups)?;
        }
        Ok(Self {
            readers: ReaderPool::new(database),
            location,
            profile,
            profile_id,
            newer_schema,
            supported,
            writer: Mutex::new(Writer {
                conn,
                writes_since_checkpoint: 0,
            }),
        })
    }

    /// The store's files.
    #[must_use]
    pub fn location(&self) -> &StoreLocation {
        &self.location
    }

    /// The id of the profile the store belongs to.
    #[must_use]
    pub fn profile_id(&self) -> ProfileId {
        self.profile_id
    }

    /// Whether writes are refused because the file has a newer schema.
    #[must_use]
    pub fn is_read_only(&self) -> bool {
        self.newer_schema.is_some()
    }

    /// Runs `work` in one `BEGIN IMMEDIATE` transaction, committing when
    /// it returns `Ok` and rolling back otherwise.
    pub(crate) fn write<T>(
        &self,
        work: impl FnOnce(&Transaction<'_>) -> Result<T, Error>,
    ) -> Result<T, Error> {
        self.ensure_writable()?;
        self.profile.ensure_live()?;
        let mut writer = self.writer();
        let tx = writer
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql)?;
        let value = work(&tx)?;
        tx.commit().map_err(sql)?;
        writer.committed();
        Ok(value)
    }

    /// Runs `work` on a pooled read-only connection.
    pub(crate) fn read<T>(
        &self,
        work: impl FnOnce(&Connection) -> Result<T, Error>,
    ) -> Result<T, Error> {
        self.readers.with(work)
    }

    /// Rebuilds the database file (`VACUUM`) and then truncates the WAL:
    /// the only place a truncating checkpoint runs.
    ///
    /// # Errors
    ///
    /// `store.schema_too_new`, `store.io`, `store.sqlite`.
    pub fn vacuum(&self) -> Result<(), Error> {
        self.ensure_writable()?;
        let writer = self.writer();
        writer.conn.execute_batch("VACUUM").map_err(sql)?;
        writer
            .conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
            .map_err(sql)
    }

    pub(crate) fn ensure_writable(&self) -> Result<(), Error> {
        match self.newer_schema {
            None => Ok(()),
            Some(found) => Err(Error::new(
                &codes::STORE_SCHEMA_TOO_NEW,
                format!(
                    "state.db is at schema version {found} but this build supports up to version {}; the store is read-only",
                    self.supported
                ),
            )
            .with_hint("update Prometheus to write to this store")),
        }
    }

    fn writer(&self) -> MutexGuard<'_, Writer> {
        // A panic mid-transaction drops (rolls back) the transaction, so
        // the connection is still sound.
        self.writer.lock().unwrap_or_else(PoisonError::into_inner)
    }

    #[cfg(test)]
    pub(crate) fn writes_since_checkpoint(&self) -> u32 {
        self.writer().writes_since_checkpoint
    }
}

impl Writer {
    fn committed(&mut self) {
        self.writes_since_checkpoint += 1;
        if self.writes_since_checkpoint >= CHECKPOINT_EVERY {
            self.checkpoint();
        }
    }

    /// A passive checkpoint never waits for readers; a failure only leaves
    /// the WAL longer until the next one.
    fn checkpoint(&mut self) {
        if let Err(e) = self
            .conn
            .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_| Ok(()))
        {
            tracing::warn!("state.db checkpoint failed: {e}");
        }
        self.writes_since_checkpoint = 0;
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        if self.newer_schema.is_none() {
            self.writer().checkpoint();
        }
    }
}

fn sidecars(database: &Path) -> [PathBuf; 3] {
    let with = |suffix: &str| {
        let mut name = database.as_os_str().to_owned();
        name.push(suffix);
        PathBuf::from(name)
    };
    [database.to_path_buf(), with("-wal"), with("-shm")]
}

fn check_identity(
    meta: &Meta,
    expected: &ProfileId,
    profile: &ProfileDir,
    database: &Path,
) -> Result<(), Error> {
    let expected_text = expected.to_string();
    if meta.profile_id.as_deref() == Some(expected_text.as_str()) {
        return Ok(());
    }
    let recorded = match (&meta.profile_id, &meta.profile_name) {
        (Some(id), Some(name)) => format!("profile id {id} (profile '{name}')"),
        (Some(id), None) => format!("profile id {id}"),
        (None, _) => "no profile id".to_owned(),
    };
    Err(Error::new(
        &codes::STORE_PROFILE_MISMATCH,
        format!(
            "{} records {recorded}, but profile '{}' has id {expected_text}; refusing to open it",
            database.display(),
            profile.name()
        ),
    )
    .with_hint("restore the profile's own state.db or move this file away"))
}

fn check_readable(meta: &Meta, supported: u32) -> Result<(), Error> {
    if meta.min_read_version <= supported {
        return Ok(());
    }
    Err(Error::new(
        &codes::STORE_SCHEMA_TOO_NEW,
        format!(
            "state.db is at schema version {} and needs a build supporting version {} to be read; this build supports up to version {supported}",
            meta.schema_version, meta.min_read_version
        ),
    )
    .with_hint("update Prometheus to open this store"))
}

fn set_journal_mode(conn: &Connection, mode: JournalMode) -> Result<(), Error> {
    let wanted = match mode {
        JournalMode::Wal => "wal",
        JournalMode::Delete => "delete",
    };
    let current: String = conn
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .map_err(sql)?;
    if current.eq_ignore_ascii_case(wanted) {
        return Ok(());
    }
    conn.query_row(&format!("PRAGMA journal_mode = {wanted}"), [], |_| Ok(()))
        .map_err(sql)
}

/// A pooled connection, returned on drop (also when the work panics).
struct Borrowed<'a> {
    pool: &'a ReaderPool,
    conn: Option<Connection>,
}

impl Drop for Borrowed<'_> {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            self.pool.state().idle.push(conn);
            self.pool.returned.notify_one();
        }
    }
}

/// At most [`MAX_READERS`] read-only connections, opened on demand.
struct ReaderPool {
    database: PathBuf,
    state: Mutex<PoolState>,
    returned: Condvar,
}

struct PoolState {
    idle: Vec<Connection>,
    open: usize,
}

impl ReaderPool {
    fn new(database: PathBuf) -> Self {
        Self {
            database,
            state: Mutex::new(PoolState {
                idle: Vec::new(),
                open: 0,
            }),
            returned: Condvar::new(),
        }
    }

    fn with<T>(&self, work: impl FnOnce(&Connection) -> Result<T, Error>) -> Result<T, Error> {
        let borrowed = Borrowed {
            pool: self,
            conn: Some(self.take()?),
        };
        work(borrowed.conn.as_ref().expect("held until drop"))
    }

    fn take(&self) -> Result<Connection, Error> {
        let mut state = self.state();
        loop {
            if let Some(conn) = state.idle.pop() {
                return Ok(conn);
            }
            if state.open < MAX_READERS {
                state.open += 1;
                drop(state);
                return self.connect().inspect_err(|_| self.state().open -= 1);
            }
            state = self
                .returned
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn connect(&self) -> Result<Connection, Error> {
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_URI;
        let conn = Connection::open_with_flags(&self.database, flags).map_err(sql)?;
        conn.busy_timeout(BUSY_TIMEOUT).map_err(sql)?;
        Ok(conn)
    }

    fn state(&self) -> MutexGuard<'_, PoolState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    #[cfg(test)]
    fn open_connections(&self) -> usize {
        self.state().open
    }
}

#[cfg(test)]
mod tests;

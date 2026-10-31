//! Mapping of SQLite and filesystem failures onto the error catalog.

use std::path::Path;

use pmt_core::error::{Error, codes};
use rusqlite::ErrorCode;

/// A SQLite failure: constraint violations are the caller's input
/// (`store.constraint`), a held lock is `store.busy` (the write path
/// retries it within its budget), structural damage is `store.corrupt`,
/// disk and file problems are `store.io`, the rest is unexpected
/// (`store.sqlite`).
#[allow(clippy::needless_pass_by_value)] // used as `map_err(sql)`
pub(crate) fn sql(e: rusqlite::Error) -> Error {
    let code = match e.sqlite_error_code() {
        Some(ErrorCode::ConstraintViolation) => &codes::STORE_CONSTRAINT,
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) => &codes::STORE_BUSY,
        Some(ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase) => &codes::STORE_CORRUPT,
        Some(
            ErrorCode::SystemIoFailure
            | ErrorCode::DiskFull
            | ErrorCode::CannotOpen
            | ErrorCode::ReadOnly
            | ErrorCode::PermissionDenied,
        ) => &codes::STORE_IO,
        _ => &codes::STORE_SQLITE,
    };
    Error::new(code, format!("state.db: {e}"))
}

/// A filesystem failure on `path`.
pub(crate) fn io(path: &Path, e: &std::io::Error) -> Error {
    Error::new(&codes::STORE_IO, format!("{}: {e}", path.display()))
}

/// A column that does not hold what this build wrote.
pub(crate) fn corrupt(column: &str, detail: impl std::fmt::Display) -> Error {
    Error::new(
        &codes::STORE_SQLITE,
        format!("state.db: column {column} holds an invalid value: {detail}"),
    )
}

//! The managed server's runtime record, `<home>/run/llamacpp.json`: the
//! server's pid and OS start time (never a command-line scan), its port,
//! and the supervising process. Every process of the home reads it to
//! reuse the running server instead of starting another.

use std::path::{Path, PathBuf};

use pmt_config::daemon_local::LocalBackend;
use pmt_core::error::Error;
use serde::{Deserialize, Serialize};

use crate::download::{Owner, io_error};

/// The record's file name in `<home>/run/`.
pub const FILE: &str = "llamacpp.json";

/// The running managed server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerRecord {
    /// The server's pid.
    pub pid: u32,
    /// Its OS start time.
    pub start_time: u64,
    /// Its loopback port.
    pub port: u16,
    /// The process supervising it.
    pub supervisor: Owner,
    /// The engine build directory it runs from.
    pub engine: PathBuf,
    /// The build's backend.
    pub backend: LocalBackend,
    /// RFC 3339 start time.
    pub started_at: String,
}

impl ServerRecord {
    /// The OpenAI-compatible base URL.
    #[must_use]
    pub fn base_url(&self) -> String {
        base_url(self.port)
    }

    /// Whether the recorded server process still runs (same pid, same
    /// start time).
    #[must_use]
    pub fn alive(&self) -> bool {
        pmt_core::runtime::process_start_time(self.pid) == Some(self.start_time)
    }
}

/// `http://127.0.0.1:<port>/v1`.
#[must_use]
pub fn base_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/v1")
}

/// `<home>/run/llamacpp.json`.
#[must_use]
pub fn path(home: &Path) -> PathBuf {
    pmt_core::runtime::run_dir(home).join(FILE)
}

/// The record, whether or not its process still runs.
#[must_use]
pub fn read(home: &Path) -> Option<ServerRecord> {
    serde_json::from_str(&std::fs::read_to_string(path(home)).ok()?).ok()
}

/// The record of a server that still runs.
#[must_use]
pub fn live(home: &Path) -> Option<ServerRecord> {
    read(home).filter(ServerRecord::alive)
}

/// Writes the record atomically.
///
/// # Errors
///
/// `runtime.io`.
///
/// # Panics
///
/// Never: the record serializes.
pub fn write(home: &Path, record: &ServerRecord) -> Result<(), Error> {
    let path = path(home);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| io_error(dir, &e))?;
    }
    let tmp = path.with_extension("json.tmp");
    let text = serde_json::to_vec_pretty(record).expect("a record serializes");
    std::fs::write(&tmp, text).map_err(|e| io_error(&tmp, &e))?;
    std::fs::rename(&tmp, &path).map_err(|e| io_error(&path, &e))
}

/// Removes the record (a no-op when there is none).
pub fn remove(home: &Path) {
    let _ = std::fs::remove_file(path(home));
}

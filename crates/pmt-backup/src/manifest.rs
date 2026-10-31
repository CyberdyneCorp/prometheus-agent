//! The manifest of an archive or snapshot: every member's path, size and
//! SHA-256, checked before anything is restored.

use serde::{Deserialize, Serialize};

/// Archive format version.
pub const FORMAT: u32 = 1;
/// The manifest's name inside a backup archive (its last entry).
pub const ARCHIVE_ENTRY: &str = "manifest.json";

/// One member file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    /// `/`-separated path relative to the home (backup) or profile
    /// (snapshot).
    pub path: String,
    /// Size in bytes.
    pub size: u64,
    /// Lowercase hex SHA-256 of the contents.
    pub sha256: String,
}

/// What a backup archive holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// [`FORMAT`].
    pub format: u32,
    /// RFC 3339 creation time (UTC).
    pub created_at: String,
    /// The version of prometheus that wrote it.
    pub prometheus_version: String,
    /// `--label`.
    pub label: Option<String>,
    /// Whether this is a `--quick` backup.
    pub quick: bool,
    /// Every member, sorted by path.
    pub members: Vec<Member>,
}

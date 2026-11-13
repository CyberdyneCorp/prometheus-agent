//! The desktop's workspace panes (spec: desktop-app, File browser pane;
//! Chat transcript rendering): a session's workspace listed and read
//! through the session's own terminal backend, so a container or remote
//! session's files come from where the session runs, never from a local
//! path of the client.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::Method;

/// `fs.list` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FsListParams {
    /// The session (attached).
    pub session_id: String,
    /// A directory, relative to the session's workspace (`.` its root).
    #[serde(default = "dot")]
    pub path: String,
    /// Also list entries `.gitignore` files ignore.
    #[serde(default)]
    pub show_ignored: bool,
}

fn dot() -> String {
    ".".to_owned()
}

/// One listed entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FsEntry {
    /// The file name.
    pub name: String,
    /// Its path as the session names it (relative to the workspace when
    /// inside it).
    pub path: String,
    /// A directory (expanded lazily with another `fs.list`).
    pub dir: bool,
    /// Its size in bytes, for a file.
    #[serde(default)]
    pub size: Option<u64>,
    /// Ignored by a `.gitignore` (listed only with `show_ignored`).
    #[serde(default)]
    pub ignored: bool,
}

/// `fs.list` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FsListing {
    /// The listed directory.
    pub path: String,
    /// Directories first, then files, by name.
    pub entries: Vec<FsEntry>,
}

/// `fs.read` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FsReadParams {
    /// The session (attached).
    pub session_id: String,
    /// The file, relative to the session's workspace or absolute.
    pub path: String,
    /// Read at most this many bytes from the start (default and cap
    /// 8 MiB); the pane previews the first 2 MiB.
    #[serde(default)]
    pub max_bytes: Option<u64>,
}

/// `fs.read` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FsFile {
    /// The file as the session names it.
    pub path: String,
    /// Its whole size in bytes.
    pub size: u64,
    /// Only the first `max_bytes` are in `data`.
    pub truncated: bool,
    /// `text`, `image` or `binary`.
    pub kind: String,
    /// The media type of an image (`image/png`...).
    #[serde(default)]
    pub mime: Option<String>,
    /// The bytes read, base64.
    pub data: String,
}

method!(/// `fs.list`: one directory of the session's workspace, listed
    /// through the session's own terminal backend.
    FsList = "fs.list" (FsListParams) -> FsListing);
method!(/// `fs.read`: the start of one file of the session, read through the
    /// session's own terminal backend (images the session's tools made,
    /// previews).
    FsRead = "fs.read" (FsReadParams) -> FsFile);

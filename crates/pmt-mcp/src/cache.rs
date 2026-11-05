//! The schema cache: a server's tool list as of its last listing (spec:
//! plugin-system, MCP config watch, recycling and discovery wait).
//!
//! A `lazy` server registers its tools from this file and starts on its
//! first call. A missing or unreadable cache only means the server is
//! started at discovery like any other.

use std::path::Path;

use rmcp::model::Tool;
use serde::{Deserialize, Serialize};

use crate::connection::Offers;

#[derive(Serialize, Deserialize)]
struct Cached {
    tools: Vec<Tool>,
    #[serde(default)]
    resources: bool,
    #[serde(default)]
    prompts: bool,
}

/// The cached tool list and offers at `path`.
#[must_use]
pub fn read(path: &Path) -> Option<(Vec<Tool>, Offers)> {
    let text = std::fs::read_to_string(path).ok()?;
    let cached: Cached = serde_json::from_str(&text)
        .inspect_err(|e| tracing::warn!(path = %path.display(), "MCP schema cache unreadable: {e}"))
        .ok()?;
    Some((
        cached.tools,
        Offers {
            resources: cached.resources,
            prompts: cached.prompts,
        },
    ))
}

/// Caches `tools` and `offers` at `path` (failures are logged: the cache
/// only saves a start).
pub fn write(path: &Path, tools: &[Tool], offers: Offers) {
    let cached = Cached {
        tools: tools.to_vec(),
        resources: offers.resources,
        prompts: offers.prompts,
    };
    let written = serde_json::to_vec(&cached)
        .map_err(std::io::Error::other)
        .and_then(|bytes| {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let temp = path.with_extension("tmp");
            std::fs::write(&temp, bytes)?;
            std::fs::rename(&temp, path)
        });
    if let Err(err) = written {
        tracing::warn!(path = %path.display(), "MCP schema cache not written: {err}");
    }
}

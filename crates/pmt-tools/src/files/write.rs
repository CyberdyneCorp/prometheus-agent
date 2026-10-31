//! `write_file` (spec: execution-environments, Write guards; Stale
//! overwrite protection; Post-write diagnostics).

use std::io;
use std::path::{Path, PathBuf};

use serde_json::json;

use super::backend::Kind;
use super::text;
use super::tracker::{self, Stale};
use super::{FileSession, guards, io_failure, json_result, lines};
use crate::result::{Status, ToolResult};

/// A path the guards accepted for writing.
pub(crate) struct WriteTarget {
    /// The path in the backend's namespace.
    pub(crate) path: PathBuf,
    /// The current content, or `None` for a new file.
    pub(crate) existing: Option<Vec<u8>>,
}

impl FileSession {
    /// Writes `content` to `raw`, creating it (and its parents) or
    /// replacing a file the session has fully read and that has not
    /// changed since.
    #[must_use]
    pub fn write_file(&self, raw: &str, content: &str) -> ToolResult {
        let target = match self.write_target(raw) {
            Ok(target) => target,
            Err(result) => return *result,
        };
        if let Some(existing) = &target.existing {
            if text::is_binary(existing) {
                return ToolResult::error(format!(
                    "refusing to overwrite binary file '{raw}' with text"
                ));
            }
            let stale = self.tracker().stale(&target.path, tracker::hash(existing));
            if let Some(stale) = stale {
                return stale_result(raw, stale);
            }
        }
        if let Err(err) = self.backend().write(&target.path, content.as_bytes()) {
            return io_failure(raw, &err);
        }
        self.note_written(&target.path, content);
        let lint = self.setup.linter.check(&target.path, content);
        json_result(
            Status::Ok,
            &json!({
                "path": raw,
                "bytes_written": content.len(),
                "created": target.existing.is_none(),
                "lint": lint,
            }),
        )
    }

    /// Applies the write guards to `raw` and reads what is there now.
    pub(crate) fn write_target(&self, raw: &str) -> Result<WriteTarget, Box<ToolResult>> {
        if let Some(refusal) = guards::device_namespace(raw) {
            return Err(Box::new(ToolResult::text(Status::Denied, refusal)));
        }
        let path = self.resolve(raw);
        if let Some(refusal) = self.write_refusal(&path) {
            return Err(Box::new(ToolResult::text(Status::Denied, refusal)));
        }
        let existing = match self.backend().metadata(&path) {
            Ok(meta) if meta.kind == Kind::File => self
                .backend()
                .read(&path)
                .map(Some)
                .map_err(|err| Box::new(io_failure(raw, &err)))?,
            Ok(_) => {
                return Err(Box::new(ToolResult::error(format!(
                    "'{raw}' is not a regular file"
                ))));
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => None,
            Err(err) => return Err(Box::new(io_failure(raw, &err))),
        };
        Ok(WriteTarget { path, existing })
    }

    /// The control-file and system-path guard, on the path as written and
    /// on where it leads (links resolved).
    pub(crate) fn write_refusal(&self, path: &Path) -> Option<String> {
        let resolved = guards::resolve_existing(path);
        [path, resolved.as_path()]
            .into_iter()
            .find_map(|candidate| self.setup.control.write_refusal(candidate))
    }

    /// The session now knows every line of what it wrote.
    pub(crate) fn note_written(&self, path: &Path, content: &str) {
        self.tracker().note_written(
            path,
            tracker::hash(content.as_bytes()),
            lines(content).len(),
        );
    }
}

fn stale_result(raw: &str, stale: Stale) -> ToolResult {
    let error = match stale {
        Stale::NotRead => format!(
            "'{raw}' already exists and this session has not read all of it; read it before overwriting it"
        ),
        Stale::Changed => format!(
            "'{raw}' changed on disk since this session last read or wrote it; read it again before overwriting it"
        ),
    };
    json_result(
        Status::Error,
        &json!({
            "status": "error",
            "stale_write_blocked": true,
            "path": raw,
            "error": error,
        }),
    )
}

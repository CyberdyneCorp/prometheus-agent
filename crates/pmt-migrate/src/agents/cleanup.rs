//! `prometheus migrate cleanup openclaw` (spec: hermes-migration, Source
//! archiving after import): on explicit request only, the OpenClaw
//! directories are renamed to `<name>.pre-migration` so the two agents
//! never act on the same state. Nothing is deleted, and no importer does
//! this on its own.

use std::path::{Path, PathBuf};

use pmt_core::error::{Error, codes};

/// Files whose pid names a running OpenClaw process.
const PID_FILES: &[&str] = &[
    "gateway.pid",
    "openclaw.pid",
    "gateway.lock",
    "run/gateway.pid",
];

/// The OpenClaw directories present under `user_home` (or `source`).
#[must_use]
pub fn targets(source: Option<&Path>, user_home: &Path) -> Vec<PathBuf> {
    match source {
        Some(p) => vec![p.to_owned()],
        None => super::Agent::OpenClaw.default_sources(user_home),
    }
    .into_iter()
    .filter(|p| p.is_dir())
    .collect()
}

/// The pid of a live OpenClaw process holding `dir`, if any.
#[must_use]
pub fn holder(dir: &Path) -> Option<u32> {
    PID_FILES.iter().find_map(|name| {
        let text = std::fs::read_to_string(dir.join(name)).ok()?;
        let value: serde_json::Value = serde_json::from_str(text.trim()).ok()?;
        let pid = match value {
            serde_json::Value::Number(n) => n.as_u64(),
            serde_json::Value::Object(o) => o.get("pid").and_then(serde_json::Value::as_u64),
            _ => None,
        };
        let pid = u32::try_from(pid?).ok()?;
        pmt_core::runtime::process_start_time(pid).map(|_| pid)
    })
}

/// Where `dir` is archived.
#[must_use]
pub fn archived(dir: &Path) -> PathBuf {
    let mut name = dir.file_name().unwrap_or_default().to_os_string();
    name.push(".pre-migration");
    dir.with_file_name(name)
}

/// Renames each of `dirs` to `<name>.pre-migration`.
///
/// # Errors
///
/// `migrate.source_busy` (75) while an OpenClaw process holds one (then
/// nothing is renamed), `migrate.destination` when an archive name is
/// taken or the rename fails.
pub fn archive(dirs: &[PathBuf]) -> Result<Vec<(PathBuf, PathBuf)>, Error> {
    for dir in dirs {
        if let Some(pid) = holder(dir) {
            return Err(Error::new(
                &codes::MIGRATE_SOURCE_BUSY,
                format!(
                    "OpenClaw is running (pid {pid}) and holds {}; stop it first",
                    dir.display()
                ),
            ));
        }
        if archived(dir).exists() {
            return Err(Error::new(
                &codes::MIGRATE_DESTINATION,
                format!(
                    "{} already exists; nothing was renamed",
                    archived(dir).display()
                ),
            ));
        }
    }
    let mut done = Vec::new();
    for dir in dirs {
        let to = archived(dir);
        std::fs::rename(dir, &to)
            .map_err(|e| Error::io_doing(&codes::MIGRATE_DESTINATION, "cannot rename", dir, &e))?;
        done.push((dir.clone(), to));
    }
    Ok(done)
}

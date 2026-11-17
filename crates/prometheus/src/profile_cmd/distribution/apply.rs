//! Putting a distribution's owned content into a profile: the identity
//! file, skill entries, MCP server definitions and cron jobs.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use serde::Deserialize;

use super::manifest::{self, never_installed};

/// Copies `rel` (a file or a directory) from `root` into `profile`,
/// replacing what was there; never-installed names are skipped at every
/// depth, symbolic links are never followed.
///
/// # Errors
///
/// `home.io`.
pub(super) fn replace_path(root: &Path, profile: &Path, rel: &str) -> Result<(), Error> {
    let (from, to) = (root.join(rel), profile.join(rel));
    let meta = std::fs::symlink_metadata(&from);
    if meta.as_ref().is_ok_and(std::fs::Metadata::is_dir) && to.exists() {
        std::fs::remove_dir_all(&to).map_err(|e| Error::io(&codes::HOME_IO, &to, &e))?;
    }
    match meta {
        Ok(m) if m.is_dir() => copy_dir(&from, &to),
        Ok(m) if m.is_file() => copy_file(&from, &to),
        // Owned but absent from this version: it goes away.
        _ => remove(&to),
    }
}

fn remove(path: &Path) -> Result<(), Error> {
    let result = if path.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };
    match result {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(Error::io(&codes::HOME_IO, path, &e))
        }
        _ => Ok(()),
    }
}

fn copy_file(from: &Path, to: &Path) -> Result<(), Error> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::io(&codes::HOME_IO, parent, &e))?;
    }
    let bytes = std::fs::read(from).map_err(|e| Error::io(&codes::HOME_IO, from, &e))?;
    pmt_core::private_fs::write_atomic(to, &bytes).map_err(|e| Error::io(&codes::HOME_IO, to, &e))
}

fn copy_dir(from: &Path, to: &Path) -> Result<(), Error> {
    std::fs::create_dir_all(to).map_err(|e| Error::io(&codes::HOME_IO, to, &e))?;
    let entries = std::fs::read_dir(from).map_err(|e| Error::io(&codes::HOME_IO, from, &e))?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if never_installed(&name) {
            continue;
        }
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            copy_dir(&entry.path(), &to.join(&name))?;
        } else if kind.is_file() {
            copy_file(&entry.path(), &to.join(&name))?;
        }
    }
    Ok(())
}

/// The MCP servers of `mcp.toml` (`[servers.<name>]`), as TOML values.
///
/// # Errors
///
/// `profile.distribution_invalid` for a file that does not parse.
pub(super) fn mcp_servers(root: &Path) -> Result<BTreeMap<String, toml::Value>, Error> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct File {
        #[serde(default)]
        servers: BTreeMap<String, toml::Value>,
    }
    let path = root.join(manifest::MCP);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(BTreeMap::new());
    };
    let file: File = toml::from_str(&text)
        .map_err(|e| manifest::invalid(format!("{}: {}", path.display(), e.message())))?;
    Ok(file.servers)
}

/// Writes `servers` into `mcp.servers` of `scope`'s configuration and
/// removes the previously owned ones the distribution dropped; the rest of
/// `config.toml` is left as it is.
///
/// # Errors
///
/// Config validation errors.
pub(super) fn apply_mcp(
    scope: &ProfileScope,
    servers: &BTreeMap<String, toml::Value>,
    dropped: &[String],
) -> Result<(), Error> {
    let file = scope.profile().config_file();
    for name in dropped.iter().filter(|n| !servers.contains_key(*n)) {
        file.unset(&format!("mcp.servers.{name}"))?;
    }
    let values = servers
        .iter()
        .map(|(name, value)| {
            let text = value.to_string();
            let edit = text.parse::<toml_edit::Value>().map_err(|e| {
                manifest::invalid(format!("mcp.toml servers.{name}: {}", e.message()))
            })?;
            Ok((format!("mcp.servers.{name}"), edit))
        })
        .collect::<Result<Vec<_>, Error>>()?;
    if values.is_empty() {
        return Ok(());
    }
    file.set_values(&values)
}

/// One job of `cron.toml`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DistJob {
    /// The distribution's id of the job (stable across versions).
    pub id: String,
    /// Its display name.
    #[serde(default)]
    pub name: Option<String>,
    /// Its schedule (the `cron add` grammar).
    pub schedule: String,
    /// Its prompt.
    pub prompt: String,
}

/// The jobs of `cron.toml`.
///
/// # Errors
///
/// `profile.distribution_invalid` for a file that does not parse.
pub(super) fn cron_jobs(root: &Path) -> Result<Vec<DistJob>, Error> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct File {
        #[serde(default)]
        jobs: Vec<DistJob>,
    }
    let path = root.join(manifest::CRON);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(Vec::new());
    };
    let file: File = toml::from_str(&text)
        .map_err(|e| manifest::invalid(format!("{}: {}", path.display(), e.message())))?;
    Ok(file.jobs)
}

/// Merges `jobs` into `scope`'s cron jobs by distribution id: a known job
/// gets the new schedule, prompt and name and keeps its paused state, a
/// new one is added paused, a dropped one is removed. Returns the new
/// distribution-id to job-id map.
///
/// # Errors
///
/// Invalid jobs, store errors.
pub(super) fn apply_cron(
    scope: &ProfileScope,
    launch: &pmt_core::env::LaunchEnv,
    jobs: &[DistJob],
    known: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, Error> {
    let mut merged = BTreeMap::new();
    if jobs.is_empty() && known.is_empty() {
        return Ok(merged);
    }
    let store = Arc::new(pmt_store::Store::open(scope)?);
    let runner = crate::cron_host::HostRunner::new(scope, launch, Arc::clone(&store));
    let scripts = scope.path("scripts");
    let profile = runner.profile(&scripts);
    let now = jiff::Timestamp::now();
    for job in jobs {
        let schedule = pmt_cron::schedule::Schedule::parse(&job.schedule, now, &profile.zone)
            .map_err(|e| manifest::invalid(format!("cron.toml job {}: {}", job.id, e.message())))?;
        let existing = known
            .get(&job.id)
            .filter(|id| store.cron_job(id).ok().flatten().is_some());
        let id = if let Some(id) = existing {
            pmt_cron::ops::update(
                &profile,
                id,
                job.name.as_deref(),
                |spec| {
                    spec.schedule = schedule;
                    spec.prompt.clone_from(&job.prompt);
                    Ok(())
                },
                now,
            )?;
            id.clone()
        } else {
            let spec = pmt_cron::job::JobSpec::new(schedule, job.prompt.clone());
            let new = pmt_cron::ops::NewJob {
                name: job.name.clone(),
                origin: None,
                paused: Some(format!("Added by a profile distribution (job {}).", job.id)),
            };
            pmt_cron::ops::create(&profile, spec, new, now)?
                .id()
                .to_owned()
        };
        merged.insert(job.id.clone(), id);
    }
    for (dist_id, id) in known {
        if !merged.contains_key(dist_id) && store.cron_job(id)?.is_some() {
            pmt_cron::ops::remove(&store, id)?;
        }
    }
    Ok(merged)
}

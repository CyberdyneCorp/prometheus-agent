//! Keeping other-agent imports in sync (spec: hermes-migration, Keeping
//! imports in sync): every successful Claude Code, Codex CLI or Cursor
//! import registers its source and the digest of what it read in
//! `<profile>/migrations/import-sync.toml`; `prometheus migrate sync`
//! re-runs each registered import whose digest changed, without asking.
//! Credential files are never read, so a token refresh never changes a
//! digest. The ledger makes the re-run safe: skills refresh only while
//! the user did not change them, memory and approvals merge deduplicated.

use std::path::{Path, PathBuf};

use pmt_config::ProfileName;
use pmt_config::profiles::IMPORT_SYNC_FILE;
use pmt_core::error::{Error, codes};
use serde::{Deserialize, Serialize};

use super::{Agent, Options, plan, run};
use crate::host::Host;
use crate::report::Report;

/// One registered import.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registered {
    /// The agent id.
    pub agent: String,
    /// The source directory.
    pub source: PathBuf,
    /// Digest of what the last import read.
    pub digest: String,
    /// When it last ran (RFC 3339).
    pub synced_at: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    #[serde(default, rename = "import")]
    imports: Vec<Registered>,
}

fn path(profile_dir: &Path) -> PathBuf {
    profile_dir.join(IMPORT_SYNC_FILE)
}

fn load(profile_dir: &Path) -> Result<File, Error> {
    match std::fs::read_to_string(path(profile_dir)) {
        Ok(text) => toml::from_str(&text).map_err(|e| {
            Error::new(
                &codes::MIGRATE_COMPONENT,
                format!(
                    "{} is invalid: {}",
                    path(profile_dir).display(),
                    e.message()
                ),
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(File::default()),
        Err(e) => Err(Error::io(&codes::HOME_IO, &path(profile_dir), &e)),
    }
}

fn save(profile_dir: &Path, file: &File) -> Result<(), Error> {
    let p = path(profile_dir);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| Error::io_doing(&codes::HOME_IO, "cannot create", parent, &e))?;
    }
    let text = toml::to_string(file).map_err(|e| Error::new(&codes::HOME_IO, e.to_string()))?;
    pmt_core::private_fs::write_atomic(&p, text.as_bytes())
        .map_err(|e| Error::io_doing(&codes::HOME_IO, "cannot write", &p, &e))
}

/// Registers (or refreshes) the import of `agent` from `source`.
///
/// # Errors
///
/// `home.io`, an invalid registry.
pub fn register(
    profile_dir: &Path,
    agent: Agent,
    source: &Path,
    digest: &str,
) -> Result<(), Error> {
    let mut file = load(profile_dir)?;
    let entry = Registered {
        agent: agent.id().to_owned(),
        source: source.to_owned(),
        digest: digest.to_owned(),
        synced_at: jiff::Timestamp::now().to_string(),
    };
    match file
        .imports
        .iter_mut()
        .find(|r| r.agent == entry.agent && r.source == entry.source)
    {
        Some(r) => *r = entry,
        None => file.imports.push(entry),
    }
    save(profile_dir, &file)
}

/// What a sync did with one registered import.
#[derive(Debug, Clone, Serialize)]
pub struct Synced {
    /// The agent id.
    pub agent: String,
    /// The source directory.
    pub source: String,
    /// `skipped: unchanged`, `synced`, `would sync`, `failed`,
    /// `skipped: source missing`.
    pub status: String,
    /// The import's report when it ran.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report: Option<Report>,
}

/// Syncs every import registered in `profile`.
///
/// # Errors
///
/// An invalid registry; each import's own failures are in its result.
pub fn sync(
    prometheus_home: &Path,
    profile: &ProfileName,
    user_home: &Path,
    dry_run: bool,
    host: &dyn Host,
) -> Result<Vec<Synced>, Error> {
    let profile_dir = prometheus_home.join("profiles").join(profile.as_str());
    let file = load(&profile_dir)?;
    let mut out = Vec::new();
    for r in &file.imports {
        let result = |status: &str, report| Synced {
            agent: r.agent.clone(),
            source: r.source.display().to_string(),
            status: status.to_owned(),
            report,
        };
        let Some(agent) = Agent::parse(&r.agent).filter(|a| a.syncable()) else {
            out.push(result("skipped: not syncable", None));
            continue;
        };
        if !r.source.is_dir() {
            out.push(result("skipped: source missing", None));
            continue;
        }
        let options = Options {
            agent,
            source: r.source.clone(),
            user_home: user_home.to_owned(),
            profile: profile.clone(),
            dry_run,
            overwrite: false,
            no_backup: true,
            openclaw: super::openclaw::Settings::default(),
        };
        if plan(&options).digest == r.digest {
            out.push(result("skipped: unchanged", None));
            continue;
        }
        if dry_run {
            let (report, _) = run(prometheus_home, &options, host)?;
            out.push(result("would sync", Some(report)));
            continue;
        }
        let (report, digest) = run(prometheus_home, &options, host)?;
        let status = if report.failed() {
            "failed"
        } else {
            register(&profile_dir, agent, &r.source, &digest)?;
            "synced"
        };
        out.push(result(status, Some(report)));
    }
    Ok(out)
}

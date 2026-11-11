//! Importers for other agents: `prometheus migrate from-openclaw`,
//! `from-claude-code`, `from-codex`, `from-cursor` and `migrate detect`
//! (spec: hermes-migration, Importers for other agents).
//!
//! Each importer reads its source directory only ([`read::Reader`]),
//! plans the same kind of [`Step`]s as the Hermes components with the
//! same report dispositions, and commits them through [`crate::commit`]
//! with a ledger per source (`<profile>/migrations/<agent>-ledger.toml`).
//! Credential files are never read; secrets found in MCP definitions are
//! stripped and named ([`mcp`]). A real run first snapshots the profile
//! (`pre-migration`) unless `--no-backup`. Claude Code, Codex and Cursor
//! imports register for `prometheus migrate sync` ([`sync`]).

mod build;
mod claude;
pub mod cleanup;
mod codex;
mod cursor;
pub mod mcp;
mod openclaw;
pub mod read;
pub mod steps;
pub mod sync;
pub mod transcripts;

use std::path::{Path, PathBuf};

use pmt_config::ProfileName;
use pmt_core::error::{Error, codes};
use serde::Serialize;

use crate::commit::{self, Commit, Planned, finish_planned, open_target, staging_dir};
use crate::host::Host;
use crate::ledger::Ledger;
use crate::plan::Existing;
use crate::report::{self, ProfileReport, Report, Status};

pub use openclaw::{Preset, Settings as OpenClawSettings, SkillConflict};

/// Another agent Prometheus imports from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Agent {
    /// OpenClaw (formerly Clawdbot, Moltbot).
    OpenClaw,
    /// Anthropic's Claude Code CLI.
    ClaudeCode,
    /// OpenAI's Codex CLI.
    Codex,
    /// Cursor.
    Cursor,
}

impl Agent {
    /// Every importer, in `migrate detect` order.
    pub const ALL: [Self; 4] = [Self::OpenClaw, Self::ClaudeCode, Self::Codex, Self::Cursor];

    /// The id (`claude-code`): ledger and report file prefix, provenance.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::OpenClaw => "openclaw",
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
            Self::Cursor => "cursor",
        }
    }

    /// The display name.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::OpenClaw => "OpenClaw",
            Self::ClaudeCode => "Claude Code",
            Self::Codex => "Codex CLI",
            Self::Cursor => "Cursor",
        }
    }

    /// The agent of `id`.
    #[must_use]
    pub fn parse(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.id() == id)
    }

    /// Whether `migrate sync` keeps this import in sync.
    #[must_use]
    pub const fn syncable(self) -> bool {
        !matches!(self, Self::OpenClaw)
    }

    /// The default source directories, first existing wins (OpenClaw
    /// falls back to its legacy names).
    #[must_use]
    pub fn default_sources(self, user_home: &Path) -> Vec<PathBuf> {
        let names: &[&str] = match self {
            Self::OpenClaw => &[".openclaw", ".clawdbot", ".moltbot"],
            Self::ClaudeCode => &[".claude"],
            Self::Codex => &[".codex"],
            Self::Cursor => &[".cursor"],
        };
        names.iter().map(|n| user_home.join(n)).collect()
    }

    /// The source: `given`, else the first existing default.
    ///
    /// # Errors
    ///
    /// `migrate.source_missing` (66) when it does not exist.
    pub fn source(self, given: Option<&Path>, user_home: &Path) -> Result<PathBuf, Error> {
        let candidates =
            given.map_or_else(|| self.default_sources(user_home), |p| vec![p.to_owned()]);
        candidates
            .iter()
            .find(|p| p.is_dir())
            .cloned()
            .ok_or_else(|| {
                Error::new(
                    &codes::MIGRATE_SOURCE_MISSING,
                    format!(
                        "No {} directory found at {}",
                        self.label(),
                        candidates[0].display()
                    ),
                )
            })
    }
}

/// What one import does.
#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)] // one per command-line flag
pub struct Options {
    /// The agent.
    pub agent: Agent,
    /// Its directory.
    pub source: PathBuf,
    /// The user home (for `~/.claude.json`, `~/.agents/skills`).
    pub user_home: PathBuf,
    /// The Prometheus profile it goes to.
    pub profile: ProfileName,
    /// `--dry-run`.
    pub dry_run: bool,
    /// `--overwrite`.
    pub overwrite: bool,
    /// `--no-backup`.
    pub no_backup: bool,
    /// OpenClaw's own options.
    pub openclaw: openclaw::Settings,
}

/// The plan of one import and the digest of what it read.
pub struct Plan {
    /// The steps in commit order.
    pub steps: Planned,
    /// Digest of every source file read (`migrate sync`).
    pub digest: String,
}

/// Plans an import from the source alone.
#[must_use]
pub fn plan(options: &Options) -> Plan {
    let mut b = build::Build::new(options);
    match options.agent {
        Agent::OpenClaw => openclaw::plan(&mut b),
        Agent::ClaudeCode => claude::plan(&mut b),
        Agent::Codex => codex::plan(&mut b),
        Agent::Cursor => cursor::plan(&mut b),
    }
    b.finish()
}

/// Runs an import into the Prometheus home at `prometheus_home`; also
/// returns the digest of what it read.
///
/// # Errors
///
/// Home errors; failing steps are recorded in the report.
pub fn run(
    prometheus_home: &Path,
    options: &Options,
    host: &dyn Host,
) -> Result<(Report, String), Error> {
    let Plan { steps, digest } = plan(options);
    let run_id = pmt_core::id::SessionId::new()
        .ulid()
        .to_string()
        .to_lowercase();
    let mut out = ProfileReport {
        hermes_profile: options.agent.id().to_owned(),
        target: Some(options.profile.to_string()),
        source: options.source.display().to_string(),
        error: None,
        code: None,
        components: Vec::new(),
        entries: Vec::new(),
        warnings: Vec::new(),
    };
    if options.dry_run {
        let dir = prometheus_home
            .join("profiles")
            .join(options.profile.as_str());
        let ledger = Ledger::peek(&dir, options.agent.id());
        let existing = Existing {
            dir: &dir,
            overwrite: options.overwrite,
            ledger: &ledger,
        };
        finish_planned(steps, dir.is_dir().then_some(&existing), &mut out);
    } else {
        commit_into(prometheus_home, options, host, steps, &run_id, &mut out);
    }
    let report = Report {
        schema: report::SCHEMA,
        run_id,
        source: options.source.display().to_string(),
        dry_run: options.dry_run,
        interrupted: out
            .components
            .iter()
            .any(|c| c.status == Status::Interrupted),
        profiles: vec![out],
        skipped_profiles: Vec::new(),
        warnings: Vec::new(),
    };
    Ok((report, digest))
}

fn commit_into(
    prometheus_home: &Path,
    options: &Options,
    host: &dyn Host,
    steps: Planned,
    run_id: &str,
    out: &mut ProfileReport,
) {
    let target = match open_target(prometheus_home, &options.profile) {
        Ok(t) => t,
        Err(e) => {
            out.error = Some(e.message().to_owned());
            out.code = Some(e.code().code.to_owned());
            return;
        }
    };
    if !options.no_backup {
        let label = format!("before migrate from-{}", options.agent.id());
        if let Err(e) = pmt_backup::snapshot::create(
            &target.profile,
            pmt_backup::snapshot::Reason::PreMigration,
            Some(&label),
        ) {
            out.error = Some(format!(
                "cannot snapshot the profile first: {}",
                e.message()
            ));
            return;
        }
    }
    let staging = match staging_dir(Some(&target), run_id) {
        Ok(s) => s,
        Err(e) => {
            out.error = Some(e.message().to_owned());
            return;
        }
    };
    let commit = Commit {
        target: &target,
        overwrite: options.overwrite,
        host,
        staging: staging.path(),
        run_id,
    };
    match commit.ledger(options.agent.id(), &options.source) {
        Ok((ledger, warning)) => {
            out.warnings.extend(warning);
            commit.all(&ledger, steps, out);
        }
        Err(e) => out.error = Some(e.message().to_owned()),
    }
    let report = Report {
        schema: report::SCHEMA,
        run_id: run_id.to_owned(),
        source: options.source.display().to_string(),
        dry_run: false,
        interrupted: false,
        profiles: vec![out.clone()],
        skipped_profiles: Vec::new(),
        warnings: Vec::new(),
    };
    commit::save(&target, options.agent.id(), &report);
}

/// One source `migrate detect` found.
#[derive(Debug, Clone, Serialize)]
pub struct Detected {
    /// The agent.
    pub agent: Agent,
    /// Its directory.
    pub source: String,
    /// Items per component it would import.
    pub counts: std::collections::BTreeMap<crate::Component, usize>,
}

/// The sources present under `user_home`, each with what it would import
/// (read only).
#[must_use]
pub fn detect(user_home: &Path, profile: &ProfileName) -> Vec<Detected> {
    let mut found = Vec::new();
    for agent in Agent::ALL {
        let Ok(source) = agent.source(None, user_home) else {
            continue;
        };
        let options = Options {
            agent,
            source: source.clone(),
            user_home: user_home.to_owned(),
            profile: profile.clone(),
            dry_run: true,
            overwrite: false,
            no_backup: true,
            openclaw: openclaw::Settings::default(),
        };
        let mut counts = std::collections::BTreeMap::new();
        for (component, step) in plan(&options).steps {
            if let Ok(mut step) = step {
                *counts.entry(component).or_insert(0) += step.entries().len();
            }
        }
        found.push(Detected {
            agent,
            source: source.display().to_string(),
            counts,
        });
    }
    found
}

//! `prometheus experience` (spec: experience-library, Library CLI;
//! Retention cap; Export and import between profiles; Restore on resume
//! and repair). Reading, pinning, forgetting and restoring entries is the
//! user's own business, so these commands work whether or not
//! `evolve.enabled` is set; only `import` and `rebuild`, which add
//! entries, need it (the library exists only where evolution is on).

pub mod rpc;
pub mod slash;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use clap::Subcommand;
use pmt_config::ProfileScope;
use pmt_core::error::Error;
use pmt_evolve::experience::archive::{self, ImportRules};
use pmt_evolve::experience::entry::EntryMeta;
use pmt_evolve::experience::{Library, Settings, domain, ops};
use serde_json::json;

use crate::context::Context;
use crate::output::{json, print_raw};

/// `prometheus experience` subcommands.
#[derive(Debug, Clone, Subcommand)]
pub enum ExperienceCommand {
    /// The library's entries: id, domain, status, confidence, pinned,
    /// times shown and cue.
    List {
        /// Only this domain.
        #[arg(long, value_name = "D")]
        domain: Option<String>,
        /// Also rolled-back entries.
        #[arg(long)]
        all: bool,
    },
    /// One entry: its `entry.toml` and `EXPERIENCE.md`.
    Show {
        /// The entry.
        entry_id: String,
    },
    /// The domains, with their entry counts.
    Domains,
    /// Keep an entry: never evicted, never demoted by a rollback.
    Pin {
        /// The entry.
        entry_id: String,
    },
    /// Let an entry be evicted and demoted again.
    Unpin {
        /// The entry.
        entry_id: String,
    },
    /// Remove an entry (its harness version stays).
    Forget {
        /// The entry.
        entry_id: String,
    },
    /// Set a rolled-back entry back to active.
    Restore {
        /// The entry.
        entry_id: String,
    },
    /// Write a domain's entries to an archive another profile can import.
    Export {
        /// The domain.
        domain: String,
        /// The archive to write (`.tar.zst`).
        file: PathBuf,
    },
    /// Add an archive's entries (all checked first; any failure refuses
    /// the whole archive).
    Import {
        /// The archive.
        file: PathBuf,
        /// The domain to file them under (default: the archive's).
        #[arg(long, value_name = "D")]
        domain: Option<String>,
    },
    /// Re-create missing entries of adopted proposals (no model call).
    Rebuild {
        /// Only this domain.
        #[arg(long, value_name = "D")]
        domain: Option<String>,
    },
}

/// The library settings of `scope`.
#[must_use]
pub fn settings(scope: &ProfileScope) -> Settings {
    let config = scope.config();
    Settings::of(
        &config.evolve,
        &config.experience,
        scope.secrets().redactor().unwrap_or_default(),
    )
}

/// Runs one `experience` subcommand.
///
/// # Errors
///
/// `experience.entry_not_found` (1), `experience.invalid_domain` (2),
/// `experience.import_rejected` (65), `evolve.disabled` (78) for `import`
/// and `rebuild`, `experience.io` (74).
pub fn run(command: &ExperienceCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let dir = scope.dir();
    match command {
        ExperienceCommand::List { domain, all } => list(ctx, dir, domain.as_deref(), *all),
        ExperienceCommand::Show { entry_id } => show(ctx, dir, entry_id),
        ExperienceCommand::Domains => domains(ctx, dir),
        ExperienceCommand::Pin { entry_id } => {
            changed(ctx, &ops::pin(dir, entry_id, true)?, "pinned")
        }
        ExperienceCommand::Unpin { entry_id } => {
            changed(ctx, &ops::pin(dir, entry_id, false)?, "unpinned")
        }
        ExperienceCommand::Forget { entry_id } => {
            changed(ctx, &ops::forget(dir, entry_id)?, "forgotten")
        }
        ExperienceCommand::Restore { entry_id } => {
            changed(ctx, &ops::restore(dir, entry_id)?, "restored")
        }
        ExperienceCommand::Export { domain, file } => export(ctx, dir, domain, file),
        ExperienceCommand::Import { file, domain } => import(ctx, &scope, file, domain.as_deref()),
        ExperienceCommand::Rebuild { domain } => rebuild(ctx, &scope, domain.as_deref()),
    }
}

/// The entries `list` shows.
///
/// # Errors
///
/// `experience.invalid_domain`.
pub fn listed(dir: &Path, domain: Option<&str>, all: bool) -> Result<Vec<EntryMeta>, Error> {
    let library = Library::of(dir);
    let entries = match domain {
        Some(name) => library.entries(&domain::parse(name)?),
        None => library.all(),
    };
    Ok(entries.into_iter().filter(|e| all || e.visible()).collect())
}

fn list(ctx: &Context<'_>, dir: &Path, domain: Option<&str>, all: bool) -> Result<u8, Error> {
    let entries = listed(dir, domain, all)?;
    if ctx.json {
        return json(&entries);
    }
    let mut text = String::new();
    for e in &entries {
        let _ = writeln!(
            text,
            "{}  {}  {}  {}  {}  shown {}  {}",
            e.entry_id,
            e.domain,
            e.status.as_str(),
            e.confidence.as_str(),
            if e.pinned { "pinned" } else { "-" },
            e.times_shown,
            e.cue
        );
    }
    if entries.is_empty() {
        text.push_str("no experience entries\n");
    }
    print_raw(&text)
}

fn show(ctx: &Context<'_>, dir: &Path, id: &str) -> Result<u8, Error> {
    let library = Library::of(dir);
    let meta = library.find(id)?;
    let document = library.document(&meta);
    if ctx.json {
        return json(&json!({"entry": meta, "document": document}));
    }
    let toml = toml::to_string(&meta).unwrap_or_default();
    print_raw(&format!("{toml}\n{document}"))
}

/// Every domain with its entry count.
#[must_use]
pub fn domain_counts(dir: &Path) -> Vec<(String, usize)> {
    let library = Library::of(dir);
    library
        .domains()
        .into_iter()
        .map(|d| {
            let n = library.entries(&d).len();
            (d, n)
        })
        .collect()
}

fn domains(ctx: &Context<'_>, dir: &Path) -> Result<u8, Error> {
    let counts = domain_counts(dir);
    if ctx.json {
        let rows: Vec<_> = counts
            .iter()
            .map(|(name, entries)| json!({"name": name, "entries": entries}))
            .collect();
        return json(&rows);
    }
    let mut text = String::new();
    for (name, n) in &counts {
        let _ = writeln!(
            text,
            "{name}  {n} entr{}",
            if *n == 1 { "y" } else { "ies" }
        );
    }
    if counts.is_empty() {
        text.push_str("no experience domains\n");
    }
    print_raw(&text)
}

fn changed(ctx: &Context<'_>, meta: &EntryMeta, what: &str) -> Result<u8, Error> {
    if ctx.json {
        return json(meta);
    }
    print_raw(&format!("{} {what}\n", meta.entry_id))
}

fn export(ctx: &Context<'_>, dir: &Path, name: &str, file: &Path) -> Result<u8, Error> {
    let name = domain::parse(name)?;
    let n = archive::export(dir, &name, file)?;
    if ctx.json {
        return json(&json!({"domain": name, "entries": n, "file": file}));
    }
    print_raw(&format!(
        "exported {n} entr{} of {name} to {}\n",
        if n == 1 { "y" } else { "ies" },
        file.display()
    ))
}

fn import(
    ctx: &Context<'_>,
    scope: &ProfileScope,
    file: &Path,
    domain: Option<&str>,
) -> Result<u8, Error> {
    pmt_evolve::gate::require_enabled(scope.config(), scope.name().as_str())?;
    let rules = crate::harness_host::rules(scope, ctx.launch.user_home().map(PathBuf::as_path));
    let settings = settings(scope);
    let checks = ImportRules {
        rules: &rules,
        settings: &settings,
    };
    let entries = archive::import(scope.dir(), file, domain, checks)?;
    if ctx.json {
        return json(&entries);
    }
    let mut text = format!(
        "imported {} entr{}\n",
        entries.len(),
        if entries.len() == 1 { "y" } else { "ies" }
    );
    for e in &entries {
        let _ = writeln!(text, "{}  {}", e.entry_id, e.domain);
    }
    print_raw(&text)
}

fn rebuild(ctx: &Context<'_>, scope: &ProfileScope, domain: Option<&str>) -> Result<u8, Error> {
    pmt_evolve::gate::require_enabled(scope.config(), scope.name().as_str())?;
    let domain = domain.map(domain::parse).transpose()?;
    let restored = ops::rebuild(scope.dir(), &settings(scope), domain.as_deref())?;
    if ctx.json {
        return json(&json!({"restored": restored}));
    }
    let mut text = format!(
        "restored {} entr{}\n",
        restored.len(),
        if restored.len() == 1 { "y" } else { "ies" }
    );
    for id in &restored {
        let _ = writeln!(text, "{id}");
    }
    print_raw(&text)
}

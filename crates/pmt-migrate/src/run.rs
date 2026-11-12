//! One migration run: select the Hermes profiles, plan every selected
//! component of each from the Hermes side alone, then (unless a dry run)
//! commit the components one by one into the Prometheus profile and save
//! the report there.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use pmt_core::error::{Error, codes};

use crate::commit::{self, Commit, Planned, Target, finish_planned, open_target, staging_dir};
use crate::component::Component;
use crate::env::{self, EnvEntry};
use crate::host::Host;
use crate::ledger::Ledger;
use crate::plan::{Existing, Source, Step};
use crate::report::{self, ProfileReport, Report, Status};
use crate::source::{self, HermesHome, SourceProfile};
use crate::yaml::{self, Doc};
use crate::{config, cron, gateway, identity, mcp, memory, plugins, secrets, sessions, skills};

/// What to migrate and how.
#[derive(Debug, Clone, Default)]
#[allow(clippy::struct_excessive_bools)] // one per command-line flag
pub struct Options {
    /// The Hermes home.
    pub hermes_home: PathBuf,
    /// `--profile` (repeatable).
    pub profiles: Vec<String>,
    /// `--all`.
    pub all: bool,
    /// `--into-profile`.
    pub into_profile: Option<String>,
    /// The selected components.
    pub components: BTreeSet<Component>,
    /// `--dry-run`.
    pub dry_run: bool,
    /// `--enable-cron`.
    pub enable_cron: bool,
    /// `--overwrite`.
    pub overwrite: bool,
}

/// The Hermes profiles `options` select, and the directories left out.
///
/// # Errors
///
/// `migrate.usage` for conflicting options, `migrate.source_missing`
/// (66) without a Hermes home, `profile.not_found` for a named Hermes
/// profile that does not exist.
pub fn select(
    options: &Options,
) -> Result<(HermesHome, Vec<SourceProfile>, Vec<report::SkippedProfile>), Error> {
    if options.all && !options.profiles.is_empty() {
        return Err(Error::new(
            &codes::MIGRATE_USAGE,
            "--profile and --all exclude each other",
        ));
    }
    if options.into_profile.is_some() && options.profiles.len() != 1 {
        return Err(Error::new(
            &codes::MIGRATE_USAGE,
            "--into-profile needs exactly one --profile",
        ));
    }
    let home = HermesHome::open(&options.hermes_home)?;
    let (live, skipped) = home.profiles();
    let wanted: Vec<String> = if options.all {
        live.iter().map(|p| p.name.clone()).collect()
    } else if options.profiles.is_empty() {
        vec![home.active_profile()]
    } else {
        options.profiles.clone()
    };
    let mut chosen = Vec::new();
    for name in wanted {
        let found = live
            .iter()
            .find(|p| p.name == name)
            .cloned()
            .ok_or_else(|| {
                Error::new(
                    &codes::PROFILE_NOT_FOUND,
                    format!(
                        "no live Hermes profile '{name}' in {}",
                        home.root().display()
                    ),
                )
            })?;
        chosen.push(found);
    }
    Ok((home, chosen, skipped))
}

/// Runs a migration into the Prometheus home at `prometheus_home` (never
/// created by a dry run).
///
/// # Errors
///
/// As [`select`]; a failing profile or component is recorded in the
/// report instead.
pub fn run(prometheus_home: &Path, options: &Options, host: &dyn Host) -> Result<Report, Error> {
    let (home, profiles, skipped) = select(options)?;
    let run_id = pmt_core::id::SessionId::new()
        .ulid()
        .to_string()
        .to_lowercase();
    let mut report = Report {
        schema: report::SCHEMA,
        run_id: run_id.clone(),
        source: home.root().display().to_string(),
        dry_run: options.dry_run,
        profiles: Vec::new(),
        skipped_profiles: skipped,
        interrupted: false,
        warnings: Vec::new(),
    };
    for profile in &profiles {
        if host.interrupted() {
            report.interrupted = true;
            break;
        }
        let mut one = migrate_profile(prometheus_home, profile, options, host, &run_id);
        report.warnings.append(&mut one.1);
        report.interrupted |= one
            .0
            .components
            .iter()
            .any(|c| c.status == Status::Interrupted);
        report.profiles.push(one.0);
        if report.interrupted {
            break;
        }
    }
    Ok(report)
}

/// The Hermes side of one profile, read once.
struct Read {
    config: Doc,
    config_error: Option<String>,
    env: Vec<EnvEntry>,
}

fn read_profile(profile: &SourceProfile) -> Read {
    let (config, config_error) = match profile.read("config.yaml") {
        Ok(Some(text)) => match yaml::parse(&text) {
            Ok(doc) => (doc, None),
            Err(why) => (
                Doc::default(),
                Some(format!("Hermes config.yaml does not parse: {why}")),
            ),
        },
        Ok(None) => (Doc::default(), None),
        Err(e) => (Doc::default(), Some(e.message().to_owned())),
    };
    let env = profile
        .read(".env")
        .ok()
        .flatten()
        .map(|t| env::parse(&t))
        .unwrap_or_default();
    Read {
        config,
        config_error,
        env,
    }
}

fn migrate_profile(
    prometheus_home: &Path,
    profile: &SourceProfile,
    options: &Options,
    host: &dyn Host,
    run_id: &str,
) -> (ProfileReport, Vec<String>) {
    let mut out = ProfileReport {
        hermes_profile: profile.name.clone(),
        target: None,
        source: profile.dir.display().to_string(),
        error: None,
        code: None,
        components: Vec::new(),
        entries: Vec::new(),
        warnings: Vec::new(),
    };
    let mut run_warnings = Vec::new();
    let target = match source::target_name(&profile.name, options.into_profile.as_deref()) {
        Ok(t) => t,
        Err(e) => {
            out.error = Some(e.message().to_owned());
            return (out, run_warnings);
        }
    };
    out.target = Some(target.to_string());
    if source::hermes_running(profile) {
        run_warnings.push(format!(
            "Hermes is running for profile {}: this run imports a snapshot, and changes made after it will be picked up by a re-run",
            profile.name
        ));
    }
    let read = read_profile(profile);
    let dest = if options.dry_run {
        None
    } else {
        match open_target(prometheus_home, &target) {
            Ok(t) => Some(t),
            Err(e) => {
                out.error = Some(e.message().to_owned());
                out.code = Some(e.code().code.to_owned());
                return (out, run_warnings);
            }
        }
    };
    let staging_root = staging_dir(dest.as_ref(), run_id);
    let staging = match &staging_root {
        Ok(dir) => dir.path().to_owned(),
        Err(e) => {
            out.error = Some(e.message().to_owned());
            return (out, run_warnings);
        }
    };
    let src = Source {
        profile,
        config: &read.config,
        env: &read.env,
        host,
        enable_cron: options.enable_cron,
        staging: &staging,
    };
    let steps = plan_all(&src, read.config_error.as_deref(), options, &mut out);
    match &dest {
        None => {
            let dir = prometheus_home.join("profiles").join(target.as_str());
            let ledger = Ledger::peek(&dir, LEDGER_SOURCE);
            let existing = Existing {
                dir: &dir,
                overwrite: options.overwrite,
                ledger: &ledger,
            };
            finish_planned(steps, dir.is_dir().then_some(&existing), &mut out);
        }
        Some(target) => {
            let commit = Commit {
                target,
                overwrite: options.overwrite,
                host,
                staging: &staging,
                run_id,
            };
            match commit.ledger(LEDGER_SOURCE, &profile.dir) {
                Ok((ledger, warning)) => {
                    out.warnings.extend(warning);
                    commit.all(&ledger, steps, &mut out);
                }
                Err(e) => out.error = Some(e.message().to_owned()),
            }
        }
    }
    drop(staging_root);
    if let Some(target) = &dest {
        save(target, &out, run_id, options, &run_warnings);
    }
    (out, run_warnings)
}

/// The ledger source name of Hermes migrations.
pub const LEDGER_SOURCE: &str = "hermes";

fn plan_all(
    src: &Source<'_>,
    config_error: Option<&str>,
    options: &Options,
    out: &mut ProfileReport,
) -> Planned {
    Component::COMMIT_ORDER
        .into_iter()
        .filter(|c| options.components.contains(c))
        .map(|c| (c, plan_one(c, src, config_error, &mut out.warnings)))
        .collect()
}

fn plan_one(
    component: Component,
    src: &Source<'_>,
    config_error: Option<&str>,
    warnings: &mut Vec<String>,
) -> Result<Box<dyn Step>, Error> {
    let boxed = |s: Box<dyn Step>| Ok(s);
    match component {
        Component::Config => match config_error {
            Some(why) => Err(Error::new(&codes::MIGRATE_COMPONENT, why)),
            None => boxed(Box::new(config::plan(src, warnings)?)),
        },
        Component::Secrets => boxed(Box::new(secrets::plan(src)?)),
        Component::Identity => boxed(Box::new(identity::plan(src)?)),
        Component::Memory => boxed(Box::new(memory::plan(src)?)),
        Component::Skills => boxed(Box::new(skills::plan(src)?)),
        Component::Sessions => boxed(Box::new(sessions::plan(src)?)),
        Component::Cron => boxed(Box::new(cron::plan(src)?)),
        Component::Mcp => boxed(Box::new(mcp::plan(src))),
        Component::Plugins => boxed(Box::new(plugins::plan(src)?)),
        Component::Gateway => boxed(Box::new(gateway::plan(src)?)),
    }
}

/// Saves one profile's report as `<profile>/migrations/hermes-<run-id>.json`.
fn save(
    target: &Target,
    out: &ProfileReport,
    run_id: &str,
    options: &Options,
    warnings: &[String],
) {
    let report = Report {
        schema: report::SCHEMA,
        run_id: run_id.to_owned(),
        source: options.hermes_home.display().to_string(),
        dry_run: false,
        profiles: vec![out.clone()],
        skipped_profiles: Vec::new(),
        interrupted: out
            .components
            .iter()
            .any(|c| c.status == Status::Interrupted),
        warnings: warnings.to_vec(),
    };
    commit::save(target, LEDGER_SOURCE, &report);
}

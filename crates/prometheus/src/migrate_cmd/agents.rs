//! `prometheus migrate from-openclaw|from-claude-code|from-codex|from-cursor`
//! and `migrate detect` (spec: hermes-migration, Importers for other
//! agents; OpenClaw mapping).

use std::fmt::Write as _;
use std::path::PathBuf;

use clap::{Args, ValueEnum};
use pmt_core::error::Error;
use pmt_migrate::agents::{self, Agent, Options, Preset, SkillConflict, sync};

use super::{confirm, show, target_profile, user_home};
use crate::context::Context;
use crate::output::{json, print_raw};

/// Options every other-agent importer takes.
#[derive(Debug, Clone, Args)]
#[allow(clippy::struct_excessive_bools)] // one per flag
pub struct AgentArgs {
    /// The agent's directory (default `~/.openclaw`, `~/.claude`,
    /// `~/.codex` or `~/.cursor`).
    #[arg(long, value_name = "PATH")]
    pub source: Option<PathBuf>,
    /// Print the per-item plan; write nothing.
    #[arg(long)]
    pub dry_run: bool,
    /// Do not ask before writing (required without a terminal).
    #[arg(long)]
    pub yes: bool,
    /// Replace items that exist in Prometheus instead of reporting a
    /// conflict.
    #[arg(long)]
    pub overwrite: bool,
    /// Skip the `pre-migration` snapshot of the profile.
    #[arg(long)]
    pub no_backup: bool,
}

/// `--preset`.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum PresetArg {
    /// Identity, memory, skills, MCP servers and the time zone.
    UserData,
    /// Also models, providers, sandbox and other infrastructure.
    Full,
}

/// `--skill-conflict`.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum SkillConflictArg {
    /// Keep the profile's skill and report a conflict.
    Skip,
    /// Replace the profile's skill.
    Overwrite,
    /// Import beside it as `<name>-imported`.
    Rename,
}

/// `migrate from-openclaw` options.
#[derive(Debug, Clone, Args)]
pub struct OpenClawArgs {
    /// The options every importer takes.
    #[command(flatten)]
    pub common: AgentArgs,
    /// What to import.
    #[arg(long, value_enum, default_value = "user-data")]
    pub preset: PresetArg,
    /// Import provider API keys into the secrets store.
    #[arg(long)]
    pub migrate_secrets: bool,
    /// Copy the workspace `AGENTS.md` into this directory.
    #[arg(long, value_name = "PATH")]
    pub workspace_target: Option<PathBuf>,
    /// What a skill that exists in Prometheus does.
    #[arg(long, value_enum, default_value = "skip")]
    pub skill_conflict: SkillConflictArg,
}

fn settings(args: Option<&OpenClawArgs>) -> agents::OpenClawSettings {
    let Some(a) = args else {
        return agents::OpenClawSettings::default();
    };
    agents::OpenClawSettings {
        preset: match a.preset {
            PresetArg::UserData => Preset::UserData,
            PresetArg::Full => Preset::Full,
        },
        migrate_secrets: a.migrate_secrets,
        workspace_target: a.workspace_target.clone(),
        skill_conflict: match a.skill_conflict {
            SkillConflictArg::Skip => SkillConflict::Skip,
            SkillConflictArg::Overwrite => SkillConflict::Overwrite,
            SkillConflictArg::Rename => SkillConflict::Rename,
        },
    }
}

/// Runs one importer: plan, print it, ask, commit, register for sync.
///
/// # Errors
///
/// `migrate.source_missing` (66), home errors.
pub(super) fn run(
    agent: Agent,
    args: &AgentArgs,
    openclaw: Option<&OpenClawArgs>,
    ctx: &Context<'_>,
) -> Result<u8, Error> {
    let user = user_home(ctx)?;
    let source = agent.source(args.source.as_deref(), &user)?;
    let home = pmt_config::home::resolve(ctx.home_flag, ctx.launch)?;
    let mut options = Options {
        agent,
        source,
        user_home: user,
        profile: target_profile(ctx, &home)?,
        dry_run: true,
        overwrite: args.overwrite,
        no_backup: args.no_backup,
        openclaw: settings(openclaw),
    };
    let host = super::host::MigrateHost::new(ctx.launch, super::interrupt::Interrupt::none());
    let (plan, _) = agents::run(&home, &options, &host)?;
    if args.dry_run {
        if ctx.json {
            return json(&plan);
        }
        print_raw(&plan.items())?;
        return Ok(plan.exit_code());
    }
    eprint!("{}", plan.items());
    let question = format!("Import these from {} into Prometheus?", agent.label());
    if let Some(code) = confirm(&question, args.yes, ctx.json)? {
        return Ok(code);
    }
    options.dry_run = false;
    let (report, digest) = agents::run(&home, &options, &host)?;
    if agent.syncable() && !report.failed() {
        let dir = home.join("profiles").join(options.profile.as_str());
        sync::register(&dir, agent, &options.source, &digest)?;
    }
    show(&report, ctx.json)?;
    Ok(report.exit_code())
}

/// `migrate detect`: the agents found and what each would import.
///
/// # Errors
///
/// An unknown user home.
pub(super) fn detect(ctx: &Context<'_>) -> Result<u8, Error> {
    let user = user_home(ctx)?;
    let home = pmt_config::home::resolve(ctx.home_flag, ctx.launch)?;
    let found = agents::detect(&user, &target_profile(ctx, &home)?);
    if ctx.json {
        return json(&serde_json::json!({ "detected": found }));
    }
    if found.is_empty() {
        return print_raw("no other agent found\n");
    }
    let mut out = String::new();
    for d in &found {
        let counts: Vec<String> = d.counts.iter().map(|(c, n)| format!("{c} {n}")).collect();
        let _ = writeln!(
            out,
            "{:<12} {}  {}",
            d.agent.id(),
            d.source,
            if counts.is_empty() {
                "nothing to import".to_owned()
            } else {
                counts.join(", ")
            }
        );
    }
    print_raw(&out)
}

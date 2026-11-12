//! `prometheus migrate` (spec: hermes-migration): the command surface
//! over `pmt-migrate`: `from-hermes` ([`hermes`]), the other-agent
//! importers and `detect` ([`agents`]), `sync`, `cleanup` and `sessions`
//! ([`more`]), plus the binary's side of a migration
//! ([`host::MigrateHost`]).

mod agents;
mod hermes;
mod host;
mod interrupt;
mod more;

use std::path::{Path, PathBuf};

use clap::Subcommand;
use pmt_config::ProfileName;
use pmt_core::error::{Error, codes};
use pmt_migrate::Report;

use crate::context::Context;
use crate::output::{json, print_raw};
use crate::prompt;

pub use agents::{AgentArgs, OpenClawArgs};
pub use hermes::FromHermesArgs;
pub use more::{CleanupArgs, SessionsArgs, SyncArgs};

/// `prometheus migrate` subcommands.
#[derive(Debug, Subcommand)]
pub enum MigrateCommand {
    /// Import a Hermes Agent home (read-only on the source): config,
    /// secrets, identity, memory, skills, sessions, cron jobs, MCP servers,
    /// plugins and gateway settings.
    FromHermes(FromHermesArgs),
    /// Import an OpenClaw home (`~/.openclaw`, or the legacy
    /// `~/.clawdbot` and `~/.moltbot`).
    #[command(name = "from-openclaw")]
    FromOpenClaw(OpenClawArgs),
    /// Import Claude Code instructions, skills, MCP servers and command
    /// permissions (`~/.claude`; credentials are never read).
    FromClaudeCode(AgentArgs),
    /// Import Codex CLI instructions, memories, skills and MCP servers
    /// (`~/.codex`; credentials are never read).
    FromCodex(AgentArgs),
    /// Import Cursor rules, skills and MCP servers (`~/.cursor`;
    /// credentials are never read).
    FromCursor(AgentArgs),
    /// List the other agents found on this machine and what each would
    /// import.
    Detect,
    /// Re-run every registered Claude Code, Codex or Cursor import whose
    /// source changed.
    Sync(SyncArgs),
    /// Archive a migrated agent's directories (`<name>.pre-migration`).
    Cleanup(CleanupArgs),
    /// Import one Claude Code or Codex CLI conversation as a session.
    Sessions(SessionsArgs),
}

/// Runs a `migrate` subcommand; `profiles` are the `--profile` values.
///
/// # Errors
///
/// `migrate.unknown_component` (2), `migrate.usage` (2),
/// `migrate.source_missing` (66), `migrate.confirmation_required` (4),
/// home errors.
pub fn run(command: &MigrateCommand, ctx: &Context<'_>, profiles: &[String]) -> Result<u8, Error> {
    use pmt_migrate::agents::Agent;
    if !matches!(command, MigrateCommand::FromHermes(_)) && profiles.len() > 1 {
        return Err(Error::new(
            &codes::MIGRATE_USAGE,
            "this command imports into one profile; give at most one --profile",
        ));
    }
    match command {
        MigrateCommand::FromHermes(args) => hermes::run(args, ctx, profiles),
        MigrateCommand::FromOpenClaw(args) => {
            agents::run(Agent::OpenClaw, &args.common, Some(args), ctx)
        }
        MigrateCommand::FromClaudeCode(args) => agents::run(Agent::ClaudeCode, args, None, ctx),
        MigrateCommand::FromCodex(args) => agents::run(Agent::Codex, args, None, ctx),
        MigrateCommand::FromCursor(args) => agents::run(Agent::Cursor, args, None, ctx),
        MigrateCommand::Detect => agents::detect(ctx),
        MigrateCommand::Sync(args) => more::sync(args, ctx),
        MigrateCommand::Cleanup(args) => more::cleanup(args, ctx),
        MigrateCommand::Sessions(args) => more::sessions(args, ctx),
    }
}

/// Prints a report: the summary, or the whole report under `--json`.
fn show(report: &Report, as_json: bool) -> Result<u8, Error> {
    for warning in &report.warnings {
        tracing::warn!("{warning}");
    }
    if as_json {
        return json(report);
    }
    print_raw(&report.summary())
}

/// The user home (`~`), where other agents keep their directories.
fn user_home(ctx: &Context<'_>) -> Result<PathBuf, Error> {
    ctx.launch.user_home().cloned().ok_or_else(|| {
        Error::new(
            &codes::MIGRATE_SOURCE_MISSING,
            "the user home is unknown; give --source",
        )
    })
}

/// The Prometheus profile an import goes to: `--profile`, else the
/// active one (`default` before the home exists).
fn target_profile(ctx: &Context<'_>, home: &Path) -> Result<ProfileName, Error> {
    if let Some(name) = ctx.profile {
        return ProfileName::parse(name);
    }
    // Read without opening the home, which would create its layout.
    match std::fs::read_to_string(home.join(pmt_config::layout::ACTIVE_PROFILE_FILE)) {
        Ok(name) if !name.trim().is_empty() => ProfileName::parse(name.trim()),
        _ => Ok(ProfileName::default_profile()),
    }
}

/// Asks before writing: `Ok(None)` to go on, else the exit status (4
/// without a terminal and without `--yes`, 1 when declined).
fn confirm(question: &str, yes: bool, as_json: bool) -> Result<Option<u8>, Error> {
    if yes {
        return Ok(None);
    }
    if !prompt::stdin_is_tty() {
        let err = Error::new(
            &codes::MIGRATE_CONFIRMATION_REQUIRED,
            "refusing to migrate non-interactively without --yes",
        )
        .with_hint("rerun with --yes, or --dry-run to see the plan");
        crate::output::print_error(&err, as_json);
        return Ok(Some(pmt_core::exit::APPROVAL_REQUIRED));
    }
    if prompt::confirm(question)? {
        Ok(None)
    } else {
        eprintln!("nothing was changed");
        Ok(Some(pmt_core::exit::FAILURE))
    }
}

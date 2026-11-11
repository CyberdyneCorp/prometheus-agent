//! `prometheus migrate from-hermes` (spec: hermes-migration): the command
//! surface over `pmt-migrate`, plus the binary's side of a migration
//! ([`host::MigrateHost`]).

mod host;

use std::path::PathBuf;

use clap::{Args, Subcommand};
use pmt_core::error::{Error, codes};
use pmt_migrate::{Options, Report};

use crate::context::Context;
use crate::output::{json, print_raw};
use crate::prompt;

/// `prometheus migrate` subcommands.
#[derive(Debug, Subcommand)]
pub enum MigrateCommand {
    /// Import a Hermes Agent home (read-only on the source): config,
    /// secrets, identity, memory, skills, sessions, cron jobs, MCP servers,
    /// plugins and gateway settings.
    FromHermes(FromHermesArgs),
}

/// `migrate from-hermes` options (the global `--profile`, repeatable here,
/// names the Hermes profiles).
#[derive(Debug, Clone, Args)]
#[allow(clippy::struct_excessive_bools)] // one per flag
pub struct FromHermesArgs {
    /// The Hermes home (default `~/.hermes`, `%LOCALAPPDATA%\hermes` on
    /// Windows; `HERMES_HOME` is never read).
    #[arg(long, value_name = "PATH")]
    pub hermes_home: Option<PathBuf>,
    /// Every live Hermes profile.
    #[arg(long)]
    pub all: bool,
    /// The Prometheus profile a single `--profile` goes to.
    #[arg(long, value_name = "NAME")]
    pub into_profile: Option<String>,
    /// Only these components (comma-separated, repeatable).
    #[arg(long, value_name = "COMPONENTS")]
    pub only: Vec<String>,
    /// All components but these (comma-separated, repeatable).
    #[arg(long, value_name = "COMPONENTS")]
    pub skip: Vec<String>,
    /// Print the plan; write nothing (not even the Prometheus home).
    #[arg(long)]
    pub dry_run: bool,
    /// Do not ask before writing (required without a terminal).
    #[arg(long)]
    pub yes: bool,
    /// Keep the jobs Hermes had enabled enabled (stop the Hermes
    /// scheduler first).
    #[arg(long)]
    pub enable_cron: bool,
    /// Replace destination values that differ instead of reporting a
    /// conflict.
    #[arg(long)]
    pub overwrite: bool,
}

/// Runs a `migrate` subcommand; `profiles` are the `--profile` values.
///
/// # Errors
///
/// `migrate.unknown_component` (2), `migrate.usage` (2),
/// `migrate.source_missing` (66), `migrate.confirmation_required` (4),
/// home errors.
pub fn run(command: &MigrateCommand, ctx: &Context<'_>, profiles: &[String]) -> Result<u8, Error> {
    let MigrateCommand::FromHermes(args) = command;
    let options = options(args, ctx, profiles)?;
    // The source is checked before anything is asked or written.
    pmt_migrate::run::select(&options)?;
    let home = pmt_config::home::resolve(ctx.home_flag, ctx.launch)?;
    let host = host::MigrateHost::new(ctx.launch);
    if !options.dry_run && !args.yes {
        if !prompt::stdin_is_tty() {
            let err = Error::new(
                &codes::MIGRATE_CONFIRMATION_REQUIRED,
                "refusing to migrate non-interactively without --yes",
            )
            .with_hint("rerun with --yes, or --dry-run to see the plan");
            crate::output::print_error(&err, ctx.json);
            return Ok(pmt_core::exit::APPROVAL_REQUIRED);
        }
        let plan = pmt_migrate::run(
            &home,
            &Options {
                dry_run: true,
                ..options.clone()
            },
            &host,
        )?;
        eprint!("{}", plan.summary());
        if !prompt::confirm("Migrate these into Prometheus?")? {
            eprintln!("nothing was migrated");
            return Ok(pmt_core::exit::FAILURE);
        }
    }
    let report = pmt_migrate::run(&home, &options, &host)?;
    show(&report, ctx.json)?;
    Ok(if report.failed() {
        pmt_core::exit::FAILURE
    } else {
        pmt_core::exit::SUCCESS
    })
}

fn options(
    args: &FromHermesArgs,
    ctx: &Context<'_>,
    profiles: &[String],
) -> Result<Options, Error> {
    let components = pmt_migrate::component::select(&args.only, &args.skip)?;
    let hermes_home = if let Some(path) = &args.hermes_home {
        path.clone()
    } else {
        let user = ctx.launch.user_home().ok_or_else(|| {
            Error::new(
                &codes::MIGRATE_SOURCE_MISSING,
                "the user home is unknown; give --hermes-home",
            )
        })?;
        pmt_migrate::default_hermes_home(user, ctx.launch.local_app_data().map(PathBuf::as_path))
    };
    Ok(Options {
        hermes_home,
        profiles: profiles.to_vec(),
        all: args.all,
        into_profile: args.into_profile.clone(),
        components,
        dry_run: args.dry_run,
        enable_cron: args.enable_cron,
        overwrite: args.overwrite,
    })
}

fn show(report: &Report, as_json: bool) -> Result<u8, Error> {
    for warning in &report.warnings {
        tracing::warn!("{warning}");
    }
    if as_json {
        return json(report);
    }
    print_raw(&report.summary())
}

//! `prometheus migrate from-hermes` (spec: hermes-migration).

use std::path::PathBuf;

use clap::Args;
use pmt_core::error::{Error, codes};
use pmt_migrate::Options;

use super::show;
use crate::context::Context;
use crate::prompt;

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

/// Runs `migrate from-hermes`; `profiles` are the `--profile` values.
///
/// # Errors
///
/// `migrate.unknown_component` (2), `migrate.usage` (2),
/// `migrate.source_missing` (66), `migrate.confirmation_required` (4),
/// home errors.
pub(super) fn run(
    args: &FromHermesArgs,
    ctx: &Context<'_>,
    profiles: &[String],
) -> Result<u8, Error> {
    let options = options(args, ctx, profiles)?;
    // The source is checked before anything is asked or written.
    pmt_migrate::run::select(&options)?;
    let home = pmt_config::home::resolve(ctx.home_flag, ctx.launch)?;
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
            &super::host::MigrateHost::new(ctx.launch, super::interrupt::Interrupt::none()),
        )?;
        eprint!("{}", plan.summary());
        if !prompt::confirm("Migrate these into Prometheus?")? {
            eprintln!("nothing was migrated");
            return Ok(pmt_core::exit::FAILURE);
        }
    }
    // Ctrl+C now stops the run at the next commit boundary (exit 130).
    let host = super::host::MigrateHost::new(ctx.launch, super::interrupt::Interrupt::listen());
    let report = pmt_migrate::run(&home, &options, &host)?;
    show(&report, ctx.json)?;
    Ok(report.exit_code())
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

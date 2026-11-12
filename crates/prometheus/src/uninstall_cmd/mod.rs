//! `prometheus uninstall [--purge | --data-only] [--desktop-data]
//! [--dry-run] [--yes]` (spec: distribution-and-update, Uninstall;
//! Uninstall scopes and dry run).
//!
//! - default: the binary, its `pmt` alias, `prometheus.prev` and the
//!   install receipt, after stopping the home's daemon and removing the
//!   service units `prometheus daemon install-service` created; the home
//!   is kept
//! - `--purge`: also the whole `PROMETHEUS_HOME`
//! - `--data-only`: keep the binary, remove the contents of the home
//! - `--desktop-data`: the desktop app's per-user data directories
//! - `--dry-run`: print every file, directory, unit and process it would
//!   remove or stop; change nothing
//!
//! Deleting data asks first (`--yes` without a terminal, else exit 4).
//! Removing a binary another owner installed (package managers, Nix,
//! Termux, the image, desktop packages, source builds) exits 69 naming
//! how that owner removes it; `--data-only` and `--desktop-data` still
//! work there. Shell completions are printed by `prometheus completions`,
//! never installed, so there are none to remove.

mod plan;

use std::path::Path;

use clap::Args;
use pmt_core::error::{Error, codes};
use pmt_update::Owner;

use crate::context::Context;
use plan::Plan;

/// `prometheus uninstall` arguments.
#[derive(Debug, Clone, Args)]
#[allow(clippy::struct_excessive_bools)] // independent switches of one command line
pub struct UninstallArgs {
    /// Also delete `PROMETHEUS_HOME` (asks first; `--yes` without a
    /// terminal).
    #[arg(long, conflicts_with = "data_only")]
    pub purge: bool,
    /// Keep the binary; delete the contents of `PROMETHEUS_HOME` (asks
    /// first). Allowed for installs owned by a package manager.
    #[arg(long)]
    pub data_only: bool,
    /// Delete the desktop app's per-user data: webview storage, window
    /// state and desktop logs.
    #[arg(long)]
    pub desktop_data: bool,
    /// Print everything it would remove or stop; change nothing.
    #[arg(long)]
    pub dry_run: bool,
    /// Confirm deleting data without asking.
    #[arg(long, short = 'y')]
    pub yes: bool,
}

impl UninstallArgs {
    /// The binary goes unless only data was asked for.
    fn removes_binary(&self) -> bool {
        self.purge || !(self.data_only || self.desktop_data)
    }

    fn removes_home(&self) -> bool {
        self.purge || self.data_only
    }
}

/// Runs `prometheus uninstall`.
///
/// # Errors
///
/// `uninstall.owned_elsewhere` (69), file errors (74); a missing
/// confirmation exits 4.
pub fn run(args: &UninstallArgs, ctx: &Context<'_>) -> Result<u8, Error> {
    let install = crate::update_cmd::install(ctx)?;
    let owner = install.receipt()?.map_or(Owner::Dev, |r| r.owner);
    if args.removes_binary() && !owner.self_updates() {
        return Err(owned_elsewhere(owner));
    }
    let plan = Plan::build(args, ctx, &install.binary, install.home.root())?;
    if args.dry_run {
        print!("{}", plan.render(true));
        println!("nothing was changed (dry run)");
        return Ok(pmt_core::exit::SUCCESS);
    }
    if plan.deletes_data() {
        match confirm(args, &plan, install.home.root())? {
            Answer::Yes => {}
            Answer::No => {
                println!("uninstall cancelled; nothing was removed");
                return Ok(pmt_core::exit::SUCCESS);
            }
            Answer::Unanswerable(err) => {
                crate::output::print_error(&err, ctx.json);
                return Ok(pmt_core::exit::APPROVAL_REQUIRED);
            }
        }
    }
    let left = plan.execute(ctx)?;
    for note in &left {
        eprintln!("{note}");
    }
    Ok(pmt_core::exit::SUCCESS)
}

fn owned_elsewhere(owner: Owner) -> Error {
    Error::new(
        &codes::UNINSTALL_OWNED_ELSEWHERE,
        format!(
            "owner: {}: this install is removed by its owner, not by prometheus",
            owner.as_str()
        ),
    )
    .with_hint(format!(
        "remove it with: {}; `prometheus uninstall --data-only` still deletes the data",
        owner.remove_command()
    ))
}

/// The answer to "delete the data?".
enum Answer {
    Yes,
    No,
    /// No terminal and no `--yes` (exit 4).
    Unanswerable(Error),
}

/// Asks before deleting data: `--yes`, or a yes on the terminal.
fn confirm(args: &UninstallArgs, plan: &Plan, home: &Path) -> Result<Answer, Error> {
    if args.yes {
        return Ok(Answer::Yes);
    }
    let question = format!(
        "{}delete this Prometheus data (the sessions, memory and secrets of every profile in {})?",
        plan.render(false),
        home.display()
    );
    if !crate::prompt::stdin_is_tty() {
        return Ok(Answer::Unanswerable(
            Error::new(&codes::UNINSTALL_CONFIRMATION_REQUIRED, question)
                .with_hint("rerun with --yes to confirm, or --dry-run to only list them"),
        ));
    }
    Ok(if crate::prompt::confirm(&question)? {
        Answer::Yes
    } else {
        Answer::No
    })
}

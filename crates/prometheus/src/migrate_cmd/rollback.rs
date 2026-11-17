//! `prometheus migrate rollback --run <run-id>` (spec: hermes-migration,
//! Migration rollback): every item the run created is removed and every
//! value it replaced restored, except items changed in Prometheus since,
//! which are kept and reported `kept_modified`.

use std::fmt::Write as _;

use clap::Args;
use pmt_core::error::Error;
use pmt_migrate::rollback::{self, Disposition, Rolled};

use super::confirm;
use crate::context::Context;
use crate::output::{json, print_raw};

/// `migrate rollback` flags.
#[derive(Debug, Clone, Args)]
pub struct RollbackArgs {
    /// The run to take back (the `run_id` of its report).
    #[arg(long = "run", value_name = "RUN_ID")]
    pub run: String,
    /// Roll back without asking.
    #[arg(long)]
    pub yes: bool,
}

fn word(disposition: Disposition) -> &'static str {
    match disposition {
        Disposition::Removed => "removed",
        Disposition::Restored => "restored",
        Disposition::KeptModified => "kept_modified",
        Disposition::Gone => "gone",
    }
}

fn summary(rolled: &[Rolled]) -> String {
    let mut out = String::new();
    for profile in rolled {
        let _ = writeln!(
            out,
            "rolled back run {} in profile {}",
            profile.run, profile.profile
        );
        for d in [
            Disposition::Removed,
            Disposition::Restored,
            Disposition::KeptModified,
            Disposition::Gone,
        ] {
            let count = profile.items.iter().filter(|i| i.disposition == d).count();
            if count > 0 {
                let _ = writeln!(out, "  {}: {count}", word(d));
            }
        }
        for kept in profile
            .items
            .iter()
            .filter(|i| i.disposition == Disposition::KeptModified)
        {
            let _ = writeln!(out, "  kept_modified {} {}", kept.kind, kept.item);
        }
    }
    out
}

/// Runs the rollback.
///
/// # Errors
///
/// `migrate.run_not_found` (66), `migrate.rollback_order`; home, store
/// and secrets errors.
pub(super) fn run(args: &RollbackArgs, ctx: &Context<'_>) -> Result<u8, Error> {
    let home = ctx.home()?;
    eprintln!(
        "This removes what migration run {} created and restores what it replaced; items changed since are kept.",
        args.run
    );
    if let Some(code) = confirm("Roll it back?", args.yes, ctx.json)? {
        return Ok(code);
    }
    let rolled = rollback::rollback(home.root(), &args.run)?;
    if ctx.json {
        return json(&serde_json::json!({ "rolled_back": rolled }));
    }
    print_raw(&summary(&rolled))
}

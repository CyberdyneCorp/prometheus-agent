//! `prometheus achievements` (spec: pets, Achievements from session
//! history): scans the profile's session history (only new or changed
//! sessions are analysed) and shows every badge with its tier, count and
//! what it counts. Everything is computed and kept on this machine.

use std::fmt::Write as _;

use pmt_core::error::Error;

use crate::context::Context;

/// `prometheus achievements` flags.
#[derive(Debug, Clone, Default, clap::Args)]
pub struct AchievementsArgs {
    /// Only the badges with a tier.
    #[arg(long)]
    pub unlocked: bool,
}

/// Runs `prometheus achievements`.
///
/// # Errors
///
/// Store and file errors.
pub fn run(args: &AchievementsArgs, ctx: &Context<'_>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let achievements = crate::pets_host::achievements::profile(&scope);
    let store = pmt_store::Store::open(&scope)?;
    let outcome = achievements.scan(&store)?;
    let mut snapshot = achievements.snapshot();
    if args.unlocked {
        snapshot.badges.retain(|b| b.tier.is_some());
    }
    if ctx.json {
        return crate::output::json(&snapshot);
    }
    let mut text = format!(
        "{} sessions ({} analysed now)\n",
        snapshot.sessions, outcome.analysed
    );
    for badge in &snapshot.badges {
        let tier = badge.tier.as_deref().unwrap_or("locked");
        let next = match (&badge.next_tier, badge.next_threshold) {
            (Some(next), Some(at)) => format!(", {next} at {at}"),
            _ => String::new(),
        };
        let _ = write!(
            text,
            "\n{:<18} {:<9} {}{next}\n  {}",
            badge.name, tier, badge.value, badge.description
        );
    }
    crate::output::print(&text)
}

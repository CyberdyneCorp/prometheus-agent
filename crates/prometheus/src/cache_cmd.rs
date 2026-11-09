//! `prometheus cache stats|prune|clear` (spec: replay-and-evaluation,
//! Cache administration and limits): report and manage the profile's
//! content-addressed model-call cache.

use std::fmt::Write as _;
use std::time::Duration;

use clap::Subcommand;
use pmt_core::error::{Error, codes};
use pmt_providers::calls::cache::{self, Stats, Usage};

use crate::context::Context;
use crate::output::{document, print_raw};

/// `prometheus cache` subcommands.
#[derive(Debug, Subcommand)]
pub enum CacheCommand {
    /// Entries and bytes, in total and per namespace.
    Stats,
    /// Remove entries not used for a while.
    Prune {
        /// Remove entries last used longer ago than this (`30d`, `12h`,
        /// `90m`, `45s`).
        #[arg(long, default_value = "30d", value_parser = parse_age)]
        older_than: Duration,
        /// Only this namespace.
        #[arg(long, value_name = "NS")]
        namespace: Option<String>,
    },
    /// Remove every entry (of one namespace).
    Clear {
        /// Only this namespace.
        #[arg(long, value_name = "NS")]
        namespace: Option<String>,
    },
}

/// `<n><unit>` with unit `s`, `m`, `h`, `d` or `w`.
fn parse_age(text: &str) -> Result<Duration, String> {
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let number: u64 = number
        .parse()
        .map_err(|_| format!("'{text}' is not a duration like 30d"))?;
    let unit_secs = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        "w" => 604_800,
        _ => return Err(format!("'{text}': the unit must be s, m, h, d or w")),
    };
    Ok(Duration::from_secs(number.saturating_mul(unit_secs)))
}

/// Runs `prometheus cache`.
///
/// # Errors
///
/// No profile, cache directory I/O.
pub fn run(command: &CacheCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let dir = scope.dir();
    let failed = |err: std::io::Error| Error::io(&codes::HOME_IO, &dir.join(cache::DIR), &err);
    match command {
        CacheCommand::Stats => {
            let stats = cache::stats(dir).map_err(failed)?;
            if ctx.json {
                return document("cache stats", &stats);
            }
            print_raw(&stats_text(&stats))
        }
        CacheCommand::Prune {
            older_than,
            namespace,
        } => {
            let removed =
                cache::prune(dir, Some(*older_than), namespace.as_deref()).map_err(failed)?;
            removed_report(ctx, "cache prune", &removed)
        }
        CacheCommand::Clear { namespace } => {
            let removed = cache::prune(dir, None, namespace.as_deref()).map_err(failed)?;
            removed_report(ctx, "cache clear", &removed)
        }
    }
}

fn removed_report(ctx: &Context<'_>, command: &str, removed: &Usage) -> Result<u8, Error> {
    if ctx.json {
        return document(command, removed);
    }
    print_raw(&format!(
        "removed {} entries ({})\n",
        removed.entries,
        bytes(removed.bytes)
    ))
}

fn stats_text(stats: &Stats) -> String {
    let mut text = format!(
        "model-call cache: {} entries, {}\n",
        stats.total.entries,
        bytes(stats.total.bytes)
    );
    for (namespace, usage) in &stats.namespaces {
        let _ = writeln!(
            text,
            "  {namespace}: {} entries, {}",
            usage.entries,
            bytes(usage.bytes)
        );
    }
    text
}

#[allow(clippy::cast_precision_loss)] // display only
fn bytes(n: u64) -> String {
    match n {
        0..1_024 => format!("{n} B"),
        1_024..1_048_576 => format!("{:.1} KiB", n as f64 / 1_024.0),
        _ => format!("{:.1} MiB", n as f64 / 1_048_576.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ages_take_a_unit() {
        assert_eq!(parse_age("30d"), Ok(Duration::from_hours(720)));
        assert_eq!(parse_age("90m"), Ok(Duration::from_mins(90)));
        assert!(parse_age("30").is_err());
        assert!(parse_age("d").is_err());
    }
}

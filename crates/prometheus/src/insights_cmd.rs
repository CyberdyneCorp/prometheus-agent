//! `prometheus usage insights [--days N] [--source S] [--all-profiles]`
//! and `/insights [days]` (spec: observability, Usage insights report):
//! the report of [`pmt_obs::usage::insights`] for the active profile (or
//! every profile, each through its own scope), from the ledger and the
//! session store only: no model call and no ledger row.

use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_obs::usage::insights::{Insights, Window, insights, render};
use pmt_store::Store;
use serde::Serialize;

use crate::cli::InsightsArgs;
use crate::context::Context;

#[derive(Serialize)]
struct Profile {
    profile: String,
    #[serde(flatten)]
    insights: Insights,
}

/// Runs `prometheus usage insights`.
///
/// # Errors
///
/// `usage.unknown_source` (2), home, profile and store errors.
pub fn run(args: &InsightsArgs, ctx: &Context<'_>) -> Result<u8, Error> {
    let scopes = if args.all_profiles {
        crate::usage_cmd::every_profile(ctx)?
    } else {
        vec![ctx.scope()?]
    };
    let mut profiles = Vec::with_capacity(scopes.len());
    for scope in &scopes {
        let store = Store::open(scope)?;
        profiles.push(Profile {
            profile: scope.name().to_string(),
            insights: report(scope, &store, args.days, args.source.as_deref())?,
        });
    }
    if ctx.json {
        return crate::output::document(
            "usage insights",
            &serde_json::json!({ "profiles": profiles }),
        );
    }
    let text: Vec<String> = profiles
        .iter()
        .map(|p| {
            let body = render(&p.insights);
            if args.all_profiles {
                format!("== {} ==\n{body}", p.profile)
            } else {
                body
            }
        })
        .collect();
    crate::output::print_raw(&text.join("\n"))
}

/// The insights of `scope` over the last `days` days.
///
/// # Errors
///
/// As [`insights`].
pub fn report(
    scope: &ProfileScope,
    store: &Store,
    days: u32,
    source: Option<&str>,
) -> Result<Insights, Error> {
    let zone = pmt_obs::time::profile_zone(&scope.config().cron).unwrap_or(jiff::tz::TimeZone::UTC);
    let since = jiff::Timestamp::now() - jiff::SignedDuration::from_hours(24 * i64::from(days));
    insights(
        store,
        &Window {
            since,
            days,
            source: source.map(str::to_owned),
            zone,
        },
    )
}

/// `/insights [days]` (default 30): the same report as text.
///
/// # Errors
///
/// `usage.invalid_time` (2) for a day count that is not 1 to 3650; store
/// errors.
pub fn slash(scope: &ProfileScope, store: &Store, args: &str) -> Result<String, Error> {
    let days = match args.trim() {
        "" => 30,
        text => text
            .parse::<u32>()
            .ok()
            .filter(|d| (1..=3650).contains(d))
            .ok_or_else(|| {
                Error::new(
                    &codes::USAGE_INVALID_TIME,
                    format!("/insights takes a number of days (1 to 3650), not '{text}'"),
                )
            })?,
    };
    Ok(render(&report(scope, store, days, None)?)
        .trim_end()
        .to_owned())
}

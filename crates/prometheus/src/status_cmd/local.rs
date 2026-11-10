//! What `prometheus status` reads from the active profile itself, without
//! the daemon and without any outbound request (spec: observability,
//! Status command; Configuration-centric status): the next scheduled job,
//! today's spend against `budgets.daily_usd`, and for `--full` the
//! effective model, providers with credentials, adapters, jobs, terminal
//! backend, sandbox enforcement and session counts.

use pmt_config::ProfileScope;
use pmt_core::error::Error;
use pmt_store::{Store, UsageFilter};
use serde::Serialize;

use crate::context::Context;

/// The next run of an enabled job.
#[derive(Debug, Clone, Serialize)]
pub(super) struct NextJob {
    /// The job's name.
    pub name: String,
    /// When it runs next (RFC 3339).
    pub at: String,
}

/// Today's spend.
#[derive(Debug, Clone, Serialize)]
pub(super) struct Spend {
    /// Priced spend since midnight in the profile's zone.
    pub usd: f64,
    /// Calls without a price.
    pub unpriced_calls: u64,
    /// `budgets.daily_usd` (`None`: no daily budget).
    pub daily_usd: Option<f64>,
}

/// The profile facts every `status` shows.
#[derive(Debug, Clone, Serialize)]
pub(super) struct Basics {
    /// The active profile.
    pub profile: String,
    /// Its next scheduled job.
    pub next_job: Option<NextJob>,
    /// Its spend today.
    pub spend_today: Spend,
}

/// Every enabled job's next run, soonest first.
pub(super) fn jobs(store: &Store) -> Vec<NextJob> {
    let mut jobs: Vec<(i64, NextJob)> = store
        .cron_jobs()
        .unwrap_or_default()
        .into_iter()
        .filter(|job| job.enabled)
        .filter_map(|job| {
            let at = job.next_run_at?;
            let when = jiff::Timestamp::from_millisecond(at).ok()?;
            Some((
                at,
                NextJob {
                    name: job.name,
                    at: when.to_string(),
                },
            ))
        })
        .collect();
    jobs.sort_by_key(|(at, _)| *at);
    jobs.into_iter().map(|(_, job)| job).collect()
}

/// Midnight today in the profile's zone.
pub(super) fn today(scope: &ProfileScope) -> jiff::Timestamp {
    let zone = pmt_obs::time::profile_zone(&scope.config().cron).unwrap_or(jiff::tz::TimeZone::UTC);
    let now = jiff::Timestamp::now().to_zoned(zone);
    now.start_of_day()
        .map_or_else(|_| now.timestamp(), |day| day.timestamp())
}

/// The basics of `scope`.
///
/// # Errors
///
/// Store errors.
pub(super) fn basics(scope: &ProfileScope) -> Result<Basics, Error> {
    let store = Store::open(scope)?;
    let totals = store.usage_sum(&UsageFilter {
        since: Some(today(scope)),
        ..UsageFilter::default()
    })?;
    let daily = scope.config().budgets.daily_usd;
    Ok(Basics {
        profile: scope.name().to_string(),
        next_job: jobs(&store).into_iter().next(),
        spend_today: Spend {
            usd: totals.cost_usd,
            unpriced_calls: totals.unpriced_calls,
            daily_usd: (daily > 0.0).then_some(daily),
        },
    })
}

/// What `--full` adds.
#[derive(Debug, Clone, Serialize)]
pub(super) struct Full {
    /// `model.default` (or the provider's default).
    pub model: String,
    /// `model.provider`.
    pub provider: String,
    /// Providers whose credential is present (names, never values).
    pub providers_with_credentials: Vec<String>,
    /// The adapters the daemon would start, per served profile.
    pub adapters: Vec<(String, Vec<String>)>,
    /// Every enabled job's next run.
    pub jobs: Vec<NextJob>,
    /// `terminal.backend`.
    pub terminal_backend: String,
    /// What the OS sandbox enforces.
    pub sandbox: String,
    /// Sessions: open, started today, in all.
    pub sessions: SessionCounts,
}

/// Session counts of the profile.
#[derive(Debug, Clone, Copy, Serialize)]
pub(super) struct SessionCounts {
    /// No end recorded.
    pub active: u64,
    /// Started today.
    pub today: u64,
    /// In all.
    pub total: u64,
}

/// The `--full` facts of `scope` (local reads only).
///
/// # Errors
///
/// Store errors.
pub(super) fn full(ctx: &Context<'_>, scope: &ProfileScope) -> Result<Full, Error> {
    let config = scope.config();
    let store = Store::open(scope)?;
    let counts = store.session_counts(today(scope))?;
    let providers_with_credentials = crate::setup_cmd::sections::required_secrets(scope)
        .unwrap_or_default()
        .into_iter()
        .filter(|s| s.present)
        .map(|s| s.provider)
        .collect();
    Ok(Full {
        model: config
            .model
            .default
            .clone()
            .unwrap_or_else(|| "(provider default)".to_owned()),
        provider: config
            .model
            .provider
            .clone()
            .unwrap_or_else(|| "auto".to_owned()),
        providers_with_credentials,
        adapters: served_adapters(ctx, scope),
        jobs: jobs(&store),
        terminal_backend: config.terminal.backend.clone(),
        sandbox: crate::debug_cmd::sandbox_enforcement(ctx, scope),
        sessions: SessionCounts {
            active: counts.open,
            today: counts.since,
            total: counts.total,
        },
    })
}

/// The enabled platforms of the active profile and, with its
/// `gateway.multiplex`, of every other non-standalone profile.
fn served_adapters(ctx: &Context<'_>, scope: &ProfileScope) -> Vec<(String, Vec<String>)> {
    let enabled = |scope: &ProfileScope| {
        scope
            .config()
            .gateway
            .platforms
            .iter()
            .filter(|(_, p)| p.enabled)
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>()
    };
    let mut served = vec![(scope.name().to_string(), enabled(scope))];
    if !scope.config().gateway.multiplex {
        return served;
    }
    let Ok(home) = ctx.home() else {
        return served;
    };
    for name in home.profiles().unwrap_or_default() {
        if name.as_str() == scope.name().as_str() {
            continue;
        }
        let other = home.existing_profile(&name).and_then(|dir| {
            ProfileScope::open(
                dir,
                &pmt_config::Overrides::default(),
                pmt_config::Launch::Other,
            )
        });
        if let Ok(other) = other
            && !other.config().gateway.standalone
        {
            served.push((name.as_str().to_owned(), enabled(&other)));
        }
    }
    served
}

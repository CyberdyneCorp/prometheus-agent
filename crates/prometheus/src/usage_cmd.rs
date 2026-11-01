//! `prometheus usage` (spec: observability, Usage command; Totals keyed by
//! root lineage; Pricing and unpriced calls): totals of calls, tokens by
//! class, `cost_usd` and `unpriced_calls` from the usage ledger, grouped by
//! root, session, model, provider, purpose, day or profile.

use std::fmt::Write as _;

use jiff::Timestamp;
use jiff::tz::TimeZone;
use pmt_config::{Launch, Overrides, ProfileScope};
use pmt_core::error::{Error, codes};
use pmt_obs::usage::format_total_cost;
use pmt_obs::usage::report::{GroupBy, Report, report};
use pmt_store::{Store, UsageFilter, UsageTotals};
use serde::Serialize;

use crate::cli::{UsageArgs, UsageBy};
use crate::context::Context;
use crate::output::{document, print_raw};

/// Runs `prometheus usage`.
///
/// # Errors
///
/// `usage.invalid_time` (2), home, profile, config and store errors,
/// `session.not_found`, `output.io`.
pub fn run(args: &UsageArgs, ctx: &Context<'_>) -> Result<u8, Error> {
    let now = Timestamp::now();
    let since = point("--since", &args.since, now)?;
    let until = args
        .until
        .as_deref()
        .map(|value| point("--until", value, now))
        .transpose()?;
    let by = group(args.by);
    let scopes = if args.all_profiles {
        every_profile(ctx)?
    } else {
        vec![ctx.scope()?]
    };
    let mut profiles = Vec::with_capacity(scopes.len());
    for scope in &scopes {
        profiles.push(profile_report(scope, args, since, until, by)?);
    }
    let mut total = UsageTotals::default();
    for profile in &profiles {
        total.merge(&profile.report.total);
    }
    let body = Body {
        since: since.to_string(),
        until: until.map(|until| until.to_string()),
        by: by.as_str(),
        profiles: profiles.iter().map(ProfileJson::from).collect(),
        total: TotalsJson::from(&total),
    };
    if ctx.json {
        return document("usage", &body);
    }
    print_raw(&render(&profiles, &total, since, by, args.all_profiles))
}

fn point(flag: &str, value: &str, now: Timestamp) -> Result<Timestamp, Error> {
    pmt_obs::time::parse_point(value, now).ok_or_else(|| {
        Error::new(
            &codes::USAGE_INVALID_TIME,
            format!("invalid {flag} value '{value}': use 30m, 2h, 7d or an RFC 3339 time"),
        )
    })
}

const fn group(by: UsageBy) -> GroupBy {
    match by {
        UsageBy::Root => GroupBy::Root,
        UsageBy::Session => GroupBy::Session,
        UsageBy::Model => GroupBy::Model,
        UsageBy::Provider => GroupBy::Provider,
        UsageBy::Purpose => GroupBy::Purpose,
        UsageBy::Day => GroupBy::Day,
        UsageBy::Profile => GroupBy::Profile,
    }
}

/// Every live profile, each opened through its own `ProfileScope`.
fn every_profile(ctx: &Context<'_>) -> Result<Vec<ProfileScope>, Error> {
    let home = ctx.home()?;
    let mut scopes = Vec::new();
    for name in home.profiles()? {
        let dir = home.existing_profile(&name)?;
        scopes.push(ProfileScope::open(
            dir,
            &Overrides::default(),
            Launch::Other,
        )?);
    }
    Ok(scopes)
}

struct ProfileReport {
    name: String,
    zone: TimeZone,
    report: Report,
}

fn profile_report(
    scope: &ProfileScope,
    args: &UsageArgs,
    since: Timestamp,
    until: Option<Timestamp>,
    by: GroupBy,
) -> Result<ProfileReport, Error> {
    let zone = pmt_obs::time::profile_zone(&scope.config().cron)?;
    let store = Store::open(scope)?;
    let sessions = match &args.session {
        Some(reference) => {
            let id = store.resolve_session(reference)?;
            let mut ids = vec![id];
            ids.extend(store.tree(id)?.into_iter().map(|node| node.id));
            Some(ids)
        }
        None => None,
    };
    let filter = UsageFilter {
        since: Some(since),
        until,
        sessions,
        ..UsageFilter::default()
    };
    let name = scope.name().as_str().to_owned();
    let report = report(&store, &name, &filter, by, &zone)?;
    Ok(ProfileReport { name, zone, report })
}

#[derive(Serialize)]
struct Body<'a> {
    since: String,
    until: Option<String>,
    by: &'static str,
    profiles: Vec<ProfileJson<'a>>,
    total: TotalsJson,
}

#[derive(Serialize)]
struct ProfileJson<'a> {
    profile: &'a str,
    timezone: Option<&'a str>,
    rows: Vec<RowJson<'a>>,
    total: TotalsJson,
}

impl<'a> From<&'a ProfileReport> for ProfileJson<'a> {
    fn from(profile: &'a ProfileReport) -> Self {
        Self {
            profile: &profile.name,
            timezone: profile.zone.iana_name(),
            rows: profile
                .report
                .rows
                .iter()
                .map(|row| RowJson {
                    key: &row.key,
                    totals: TotalsJson::from(&row.totals),
                })
                .collect(),
            total: TotalsJson::from(&profile.report.total),
        }
    }
}

#[derive(Serialize)]
struct RowJson<'a> {
    key: &'a str,
    #[serde(flatten)]
    totals: TotalsJson,
}

/// Totals as the ledger names them; `cost_usd` sums priced calls only and
/// is `null` when every call was unpriced.
#[derive(Serialize)]
struct TotalsJson {
    calls: u64,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    reasoning_tokens: u64,
    cost_usd: Option<f64>,
    unpriced_calls: u64,
}

impl From<&UsageTotals> for TotalsJson {
    fn from(t: &UsageTotals) -> Self {
        Self {
            calls: t.calls,
            input_tokens: t.tokens.input,
            output_tokens: t.tokens.output,
            cache_read_tokens: t.tokens.cache_read,
            cache_write_tokens: t.tokens.cache_write,
            reasoning_tokens: t.tokens.reasoning,
            cost_usd: (t.calls == 0 || t.unpriced_calls < t.calls).then_some(t.cost_usd),
            unpriced_calls: t.unpriced_calls,
        }
    }
}

const HEADER: [&str; 8] = [
    "calls",
    "input",
    "output",
    "cache_read",
    "cache_write",
    "reasoning",
    "cost_usd",
    "unpriced_calls",
];

fn cells(totals: &UsageTotals) -> [String; 8] {
    [
        totals.calls.to_string(),
        totals.tokens.input.to_string(),
        totals.tokens.output.to_string(),
        totals.tokens.cache_read.to_string(),
        totals.tokens.cache_write.to_string(),
        totals.tokens.reasoning.to_string(),
        format_total_cost(totals),
        totals.unpriced_calls.to_string(),
    ]
}

fn line(text: &mut String, key: &str, values: &[String]) {
    let _ = write!(text, "{key:<28}");
    for (value, header) in values.iter().zip(HEADER) {
        let _ = write!(text, " {value:>width$}", width = header.len().max(10));
    }
    text.push('\n');
}

/// One table per profile (rows, then its total), a grand total row with
/// `--all-profiles`, and a closing summary line.
fn render(
    profiles: &[ProfileReport],
    total: &UsageTotals,
    since: Timestamp,
    by: GroupBy,
    all: bool,
) -> String {
    let mut text = String::new();
    let header: Vec<String> = HEADER.iter().map(|h| (*h).to_owned()).collect();
    for profile in profiles {
        let zone = profile.zone.iana_name().unwrap_or("local time");
        let _ = writeln!(
            text,
            "profile {} since {since}, by {} ({zone})",
            profile.name,
            by.as_str()
        );
        line(&mut text, by.as_str(), &header);
        for row in &profile.report.rows {
            line(&mut text, &row.key, &cells(&row.totals));
        }
        line(&mut text, "total", &cells(&profile.report.total));
        text.push('\n');
    }
    if all {
        line(&mut text, "all profiles", &cells(total));
    }
    let _ = writeln!(
        text,
        "total: {} calls, cost_usd {}, unpriced_calls: {}",
        total.calls,
        format_total_cost(total),
        total.unpriced_calls
    );
    text
}

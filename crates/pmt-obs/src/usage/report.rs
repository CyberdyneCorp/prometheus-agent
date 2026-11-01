//! Grouped usage totals for `prometheus usage` (spec: observability, Usage
//! command; Totals keyed by root lineage). Rows are summed in one pass over
//! the matching ledger rows, so the groups always add up to the total.

use std::collections::BTreeMap;

use jiff::tz::TimeZone;
use pmt_core::error::Error;
use pmt_store::{Store, UsageEvent, UsageFilter, UsageTotals};

/// What one report row groups by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupBy {
    /// Root session of the lineage.
    Root,
    /// Session that made the call.
    Session,
    /// `<provider>/<model>`.
    Model,
    /// Provider id.
    Provider,
    /// Dotted purpose.
    Purpose,
    /// Calendar day in the profile's time zone.
    Day,
    /// Profile name.
    Profile,
}

impl GroupBy {
    /// Every grouping, in `--by` order.
    pub const ALL: [Self; 7] = [
        Self::Root,
        Self::Session,
        Self::Model,
        Self::Provider,
        Self::Purpose,
        Self::Day,
        Self::Profile,
    ];

    /// The `--by` name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Root => "root",
            Self::Session => "session",
            Self::Model => "model",
            Self::Provider => "provider",
            Self::Purpose => "purpose",
            Self::Day => "day",
            Self::Profile => "profile",
        }
    }

    fn key(self, event: &UsageEvent, zone: &TimeZone, root: &str, profile: &str) -> String {
        match self {
            Self::Root => root.to_owned(),
            Self::Session => event.session_id.to_string(),
            Self::Model => format!("{}/{}", event.provider, event.model),
            Self::Provider => event.provider.clone(),
            Self::Purpose => event.purpose.clone(),
            Self::Day => event.ts.to_zoned(zone.clone()).date().to_string(),
            Self::Profile => profile.to_owned(),
        }
    }
}

/// One group's totals.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    /// The group (`2026-05-01`, `main`, `openai/gpt-x`, a session id...).
    pub key: String,
    /// Its totals.
    pub totals: UsageTotals,
}

/// Grouped totals and their sum.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Report {
    /// One row per group, ordered by key.
    pub rows: Vec<Row>,
    /// The sum of every selected ledger row.
    pub total: UsageTotals,
}

/// Sums the rows `filter` selects in `store` (a profile named `profile`),
/// grouped by `by`; days are cut in `zone`.
///
/// # Errors
///
/// `store.sqlite`.
pub fn report(
    store: &Store,
    profile: &str,
    filter: &UsageFilter,
    by: GroupBy,
    zone: &TimeZone,
) -> Result<Report, Error> {
    let mut groups: BTreeMap<String, UsageTotals> = BTreeMap::new();
    let mut total = UsageTotals::default();
    store.for_each_usage(filter, |event, root| {
        let key = by.key(&event, zone, &root.to_string(), profile);
        groups.entry(key).or_default().add(&event);
        total.add(&event);
        Ok(())
    })?;
    Ok(Report {
        rows: groups
            .into_iter()
            .map(|(key, totals)| Row { key, totals })
            .collect(),
        total,
    })
}

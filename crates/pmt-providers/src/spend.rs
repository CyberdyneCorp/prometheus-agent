//! Provider spend caps (spec: model-providers, Provider spend caps).
//!
//! `providers.<id>.daily_budget_usd` and `monthly_budget_usd` are checked
//! against the profile's usage ledger before every request to the
//! provider (calendar periods in UTC). A reached cap refuses the request
//! with class `billing` and code `provider.budget_exhausted`, so the
//! fallback chain applies; the first time a period passes 80% of a cap, a
//! WARN is emitted once for every process of the profile.

use std::sync::Arc;

use jiff::Timestamp;
use jiff::civil::Date;
use pmt_core::error::{Error, codes};
use pmt_store::Store;

use crate::registry::Provider;

/// Share of a cap that warns.
pub const WARN_AT: f64 = 0.8;

/// A cap period.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Period {
    Daily,
    Monthly,
}

impl Period {
    fn name(self) -> &'static str {
        match self {
            Self::Daily => "daily",
            Self::Monthly => "monthly",
        }
    }

    /// The period's start (UTC) and its key (`daily:2026-11-06`).
    fn start(self, now: Timestamp) -> (Timestamp, String) {
        let today = now.to_zoned(jiff::tz::TimeZone::UTC).date();
        let first = match self {
            Self::Daily => today,
            Self::Monthly => Date::new(today.year(), today.month(), 1).unwrap_or(today),
        };
        let start = first
            .to_zoned(jiff::tz::TimeZone::UTC)
            .map_or(now, |z| z.timestamp());
        let key = match self {
            Self::Daily => format!("daily:{first}"),
            Self::Monthly => format!("monthly:{}", first.strftime("%Y-%m")),
        };
        (start, key)
    }
}

/// Spend caps over a profile's ledger.
#[derive(Debug, Clone)]
pub struct SpendCaps {
    store: Arc<Store>,
}

impl SpendCaps {
    /// Caps checked against `store`'s usage ledger.
    #[must_use]
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }

    /// Whether `provider` may be sent another request at `now`.
    ///
    /// # Errors
    ///
    /// `provider.budget_exhausted` naming the period, the spend and the
    /// cap; store errors.
    pub fn check(&self, provider: &Provider, now: Timestamp) -> Result<(), Error> {
        let settings = provider.settings();
        let caps = [
            (Period::Daily, settings.daily_budget_usd),
            (Period::Monthly, settings.monthly_budget_usd),
        ];
        for (period, cap) in caps {
            let Some(cap) = cap else { continue };
            let (start, key) = period.start(now);
            let spent = self.store.provider_spent_since(provider.id(), start)?;
            if spent >= cap {
                return Err(exhausted(provider.id(), period, spent, cap));
            }
            if spent >= cap * WARN_AT && self.store.mark_budget_warning(provider.id(), &key, now)? {
                tracing::warn!(
                    provider = provider.id(),
                    period = period.name(),
                    spent_usd = spent,
                    cap_usd = cap,
                    "provider '{}' has used {:.0}% of its {} spend cap (${spent:.2} of ${cap:.2})",
                    provider.id(),
                    spent / cap * 100.0,
                    period.name()
                );
            }
        }
        Ok(())
    }
}

fn exhausted(provider: &str, period: Period, spent: f64, cap: f64) -> Error {
    let name = period.name();
    Error::new(
        &codes::PROVIDER_BUDGET_EXHAUSTED,
        format!(
            "provider '{provider}' reached its {name} spend cap: ${spent:.2} spent of ${cap:.2}"
        ),
    )
    .with_hint(format!(
        "raise it with: prometheus config set providers.{provider}.{name}_budget_usd <usd>"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn periods_start_at_utc_midnight_and_the_first_of_the_month() {
        let now: Timestamp = "2026-11-06T15:30:00Z".parse().unwrap();
        let (day, key) = Period::Daily.start(now);
        assert_eq!(day.to_string(), "2026-11-06T00:00:00Z");
        assert_eq!(key, "daily:2026-11-06");
        let (month, key) = Period::Monthly.start(now);
        assert_eq!(month.to_string(), "2026-11-01T00:00:00Z");
        assert_eq!(key, "monthly:2026-11");
    }
}

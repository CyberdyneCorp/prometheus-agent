//! The fallback chain (spec: model-providers, Fallback chain
//! configuration; In-turn fallback activation and turn-scoped restore).
//!
//! [`chain`] builds the profile's ordered, de-duplicated chain from
//! `[[fallback.chain]]`. A session's [`Fallback`] state walks it when a
//! call fails with a fallback-eligible class after same-route retries,
//! keeps the activated route for the rest of the turn, restores the
//! primary at the next turn unless the primary is cooling down, and arms
//! that cooldown when the primary is left for `rate_limit` or `billing`.

use std::collections::HashSet;
use std::time::Duration;

use jiff::{SignedDuration, Timestamp};
use pmt_config::Config;
use pmt_core::error::Error;
use serde::Serialize;

use crate::capability::{self, Needs};
use crate::classify::{ErrorClass, ProviderError};
use crate::metadata::Catalog;
use crate::registry::ProviderRegistry;
use crate::resolve::{self, Resolver, Route, Target, Trace};

/// Builds the profile's chain: entries in order, unknown providers
/// refused, duplicates (canonical provider, lower-cased model, base URL
/// without trailing slash) collapsed onto the first.
///
/// # Errors
///
/// `provider.unknown` naming the entry.
pub fn chain(config: &Config, registry: &ProviderRegistry) -> Result<Vec<Target>, Error> {
    let mut seen = HashSet::new();
    let mut targets = Vec::new();
    for (i, entry) in config.fallback.chain.iter().enumerate() {
        let provider = registry.lookup(&entry.provider).map_err(|err| {
            Error::new(
                err.code(),
                format!("fallback.chain[{i}].provider: {}", err.message()),
            )
        })?;
        let base_url = entry
            .base_url
            .as_deref()
            .or(provider.base_url())
            .unwrap_or_default();
        if !seen.insert(resolve::identity(provider.id(), &entry.model, base_url)) {
            continue;
        }
        targets.push(Target {
            provider: provider.id().to_owned(),
            model: Some(entry.model.clone()),
            base_url: entry.base_url.clone(),
            credential: entry.credential.as_ref().map(|c| c.0.clone()),
        });
    }
    Ok(targets)
}

/// Credential benches and health scores, owned by the credential-pool and
/// health tasks; the defaults (nothing benched, perfect health) apply
/// until they exist.
pub trait RouteHealth: Send + Sync {
    /// How long every pooled credential of `provider` stays benched
    /// (`None`: some credential is usable now).
    fn benched_for(&self, _provider: &str, _now: Timestamp) -> Option<Duration> {
        None
    }

    /// Health score in [0, 1].
    fn score(&self, _provider: &str, _model: &str) -> f64 {
        1.0
    }
}

/// Nothing benched, perfect health.
#[derive(Debug, Clone, Copy, Default)]
pub struct Healthy;

impl RouteHealth for Healthy {}

/// A candidate whose every credential is benched longer than this is
/// skipped.
pub const MAX_BENCH: Duration = Duration::from_mins(10);
/// First rate-limit/billing primary cooldown without a reset time.
pub const FIRST_COOLDOWN: Duration = Duration::from_mins(1);
/// Longest primary cooldown.
pub const MAX_COOLDOWN: Duration = Duration::from_hours(4);
/// Primary cooldown after the whole chain was unusable.
pub const EXHAUSTED_COOLDOWN: Duration = Duration::from_secs(5);

/// A route as a `provider.fallback` event names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RouteRef {
    /// Canonical provider id.
    pub provider: String,
    /// Model id.
    pub model: String,
}

impl RouteRef {
    fn of(route: &Route) -> Self {
        Self {
            provider: route.provider.clone(),
            model: route.model.clone(),
        }
    }
}

/// The `provider.fallback` event every surface renders as a notice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Activated {
    /// The route given up.
    pub from: RouteRef,
    /// The route now used.
    pub to: RouteRef,
    /// The failure class that caused it.
    pub reason: ErrorClass,
    /// Whether the billing source changed (subscription vs key).
    pub billing_change: bool,
}

impl Activated {
    /// The trajectory payload.
    #[must_use]
    pub fn payload(&self) -> pmt_obs::trajectory::payload::ProviderFallback {
        pmt_obs::trajectory::payload::ProviderFallback {
            from: format!("{}/{}", self.from.provider, self.from.model),
            to: format!("{}/{}", self.to.provider, self.to.model),
            reason: self.reason.as_str().to_owned(),
            billing_change: self.billing_change,
        }
    }
}

/// What a call may consult while walking the chain.
pub struct Walk<'a> {
    /// Resolves candidates.
    pub resolver: &'a Resolver<'a>,
    /// Benches and health.
    pub health: &'a dyn RouteHealth,
    /// Model capabilities.
    pub catalog: &'a Catalog,
    /// What the request needs.
    pub needs: Needs,
}

impl std::fmt::Debug for Walk<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Walk")
            .field("needs", &self.needs)
            .finish_non_exhaustive()
    }
}

/// One session's route state.
#[derive(Debug, Clone)]
pub struct Fallback {
    primary: Route,
    chain: Vec<Target>,
    /// The activated chain entry and its route; `None` = primary.
    active: Option<(usize, Route)>,
    primary_cooldown_until: Option<Timestamp>,
    consecutive_failovers: u32,
    min_health: f64,
}

impl Fallback {
    /// State for a session whose primary route is `primary`.
    #[must_use]
    pub fn new(primary: Route, chain: Vec<Target>, min_health: f64) -> Self {
        Self {
            primary,
            chain,
            active: None,
            primary_cooldown_until: None,
            consecutive_failovers: 0,
            min_health,
        }
    }

    /// The route calls use now.
    #[must_use]
    pub fn current(&self) -> &Route {
        self.active
            .as_ref()
            .map_or(&self.primary, |(_, route)| route)
    }

    /// Whether the primary route is in use.
    #[must_use]
    pub fn on_primary(&self) -> bool {
        self.active.is_none()
    }

    /// The primary cooldown end, if armed.
    #[must_use]
    pub fn primary_cooldown_until(&self) -> Option<Timestamp> {
        self.primary_cooldown_until
    }

    /// Starts a turn: restores the primary unless it is cooling down.
    /// Returns `true` when the primary was restored.
    pub fn start_turn(&mut self, now: Timestamp) -> bool {
        let cooling = self.primary_cooldown_until.is_some_and(|until| until > now);
        if self.active.is_some() && !cooling {
            self.active = None;
            return true;
        }
        false
    }

    /// A call on the primary succeeded: the consecutive-failover count
    /// starts over.
    pub fn primary_succeeded(&mut self) {
        if self.active.is_none() {
            self.consecutive_failovers = 0;
        }
    }

    /// Handles a failure of the current route (after its same-route
    /// retries): activates the next usable chain entry, or returns `None`
    /// when the class is not fallback-eligible or the chain is spent.
    pub async fn activate(
        &mut self,
        failure: &ProviderError,
        walk: &Walk<'_>,
        now: Timestamp,
    ) -> Option<Activated> {
        if !failure.class.is_fallback_eligible() {
            return None;
        }
        let failed = self.current().clone();
        let leaving_primary = self.active.is_none();
        if leaving_primary && matches!(failure.class, ErrorClass::RateLimit | ErrorClass::Billing) {
            let cooldown = failure
                .reset_at
                .filter(|reset| *reset > now)
                .unwrap_or_else(|| add(now, backoff(self.consecutive_failovers)));
            self.arm_cooldown(cooldown);
        }
        let start = self.active.as_ref().map_or(0, |(i, _)| i + 1);
        for index in start..self.chain.len() {
            let Some(route) = self.usable(index, &failed, walk, now).await else {
                continue;
            };
            if leaving_primary {
                self.consecutive_failovers += 1;
            }
            let activated = Activated {
                from: RouteRef::of(&failed),
                to: RouteRef::of(&route),
                reason: failure.class,
                billing_change: billing_kind(&failed) != billing_kind(&route),
            };
            self.active = Some((index, route));
            return Some(activated);
        }
        if leaving_primary {
            self.arm_cooldown(add(now, EXHAUSTED_COOLDOWN));
        }
        None
    }

    /// The route of chain entry `index` if it may be used now.
    async fn usable(
        &self,
        index: usize,
        failed: &Route,
        walk: &Walk<'_>,
        now: Timestamp,
    ) -> Option<Route> {
        let target = &self.chain[index];
        let route = walk
            .resolver
            .resolve_target(target, &mut Trace::default())
            .await
            .ok()?;
        let unusable = route.identity() == failed.identity()
            || walk
                .health
                .benched_for(&route.provider, now)
                .is_some_and(|d| d > MAX_BENCH)
            || walk.health.score(&route.provider, &route.model) < self.min_health
            || capability::check(
                &route.label(),
                &walk
                    .catalog
                    .metadata_at(&route.provider, &route.model, Some(&route.base_url))
                    .0,
                &walk.needs,
            )
            .is_err();
        (!unusable).then_some(route)
    }

    fn arm_cooldown(&mut self, until: Timestamp) {
        self.primary_cooldown_until =
            Some(self.primary_cooldown_until.map_or(until, |t| t.max(until)));
    }
}

/// `min(60 * 2^n, 14400)` seconds for the n-th consecutive failover.
#[must_use]
pub fn backoff(n: u32) -> Duration {
    FIRST_COOLDOWN
        .saturating_mul(2u32.saturating_pow(n))
        .min(MAX_COOLDOWN)
}

fn add(now: Timestamp, duration: Duration) -> Timestamp {
    SignedDuration::try_from(duration)
        .ok()
        .and_then(|d| now.checked_add(d).ok())
        .unwrap_or(Timestamp::MAX)
}

fn billing_kind(route: &Route) -> &'static str {
    let source = route.billing_source();
    if source.starts_with("subscription:") {
        "subscription"
    } else {
        "other"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alias_and_canonical_duplicates_collapse_and_unknown_ids_are_refused() {
        let text = "[[fallback.chain]]\nprovider = \"or\"\nmodel = \"A/B\"\n\
                    [[fallback.chain]]\nprovider = \"openrouter\"\nmodel = \"a/b\"\nbase_url = \"https://openrouter.ai/api/v1/\"\n\
                    [[fallback.chain]]\nprovider = \"deepseek\"\nmodel = \"deepseek-chat\"\n";
        let config: Config = toml::from_str(text).unwrap();
        let registry = ProviderRegistry::for_profile(&config, Vec::new()).unwrap();
        let built = chain(&config, &registry).unwrap();
        assert_eq!(built.len(), 2);
        assert_eq!(built[0].provider, "openrouter");
        assert_eq!(built[0].model.as_deref(), Some("A/B"));
        let bad: Config =
            toml::from_str("[[fallback.chain]]\nprovider = \"nope\"\nmodel = \"m\"\n").unwrap();
        let err = chain(&bad, &registry).unwrap_err();
        assert_eq!(err.code().code, "provider.unknown");
        assert!(
            err.message()
                .starts_with("fallback.chain[0].provider: Unknown provider 'nope'.")
        );
        assert!(chain(&Config::default(), &registry).unwrap().is_empty());
    }

    #[test]
    fn backoff_doubles_from_a_minute_and_caps_at_four_hours() {
        let secs: Vec<u64> = (0..10).map(|n| backoff(n).as_secs()).collect();
        assert_eq!(
            secs,
            [60, 120, 240, 480, 960, 1920, 3840, 7680, 14_400, 14_400]
        );
        assert_eq!(backoff(u32::MAX).as_secs(), 14_400);
    }
}

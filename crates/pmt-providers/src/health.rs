//! Provider health scoring (spec: model-providers, Provider health
//! scoring).
//!
//! Every sent call's outcome is kept per (provider, model, credential) in
//! `state.db`; a record covers the newest `routing.health_window` calls:
//! success rate, error counts by class, p50 and p95 time to first token
//! and p50 output tokens per second. The score in [0, 1] weighs the
//! success rate (85%) and the median time to first token (15%: full
//! below 2 s, none at 30 s or more), and an idle route's score decays
//! toward 1.0 with a half-life of `routing.health_half_life_secs`, so a
//! provider that failed an hour ago is tried again.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use jiff::Timestamp;
use pmt_config::Config;
use pmt_core::error::Error;
use pmt_store::Store;
use pmt_store::provider_state::CallSample;
use serde::Serialize;

use crate::caller::{CallObserver, Observed};
use crate::fallback::RouteHealth;
use crate::pool::PoolState;
use crate::resolve::Route;

/// Weight of the success rate in a score; the rest is speed.
const SUCCESS_WEIGHT: f64 = 0.85;
/// A median time to first token at or below this is full speed.
const FAST: f64 = 2_000.0;
/// At or above this it is no speed at all.
const SLOW: f64 = 30_000.0;

/// `[routing]`'s health settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HealthSettings {
    /// Calls a record covers.
    pub window: u32,
    /// Idle decay half-life.
    pub half_life: Duration,
}

impl HealthSettings {
    /// The profile's settings.
    #[must_use]
    pub fn of(config: &Config) -> Self {
        Self {
            window: config.routing.health_window.max(1),
            half_life: Duration::from_secs(config.routing.health_half_life_secs.max(1)),
        }
    }
}

/// One health record, as `prometheus providers health --json` prints it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Record {
    /// Provider id.
    pub provider: String,
    /// Model id.
    pub model: String,
    /// Credential id (never a value).
    pub credential: String,
    /// Calls in the window.
    pub calls: usize,
    /// Successful share of them.
    pub success_rate: f64,
    /// Failures by error class.
    pub errors: BTreeMap<String, u32>,
    /// Median time to first token.
    pub ttft_p50_ms: Option<u64>,
    /// 95th percentile time to first token.
    pub ttft_p95_ms: Option<u64>,
    /// Median output tokens per second.
    pub output_tps_p50: Option<f64>,
    /// The newest call (RFC 3339).
    pub last_call: Option<String>,
    /// The score now, after idle decay.
    pub score: f64,
}

/// The record of `samples` (newest first) at `now`.
#[must_use]
pub fn record(
    key: (&str, &str, &str),
    samples: &[CallSample],
    settings: HealthSettings,
    now: Timestamp,
) -> Record {
    let window = &samples[..samples.len().min(settings.window as usize)];
    let mut errors = BTreeMap::new();
    for class in window.iter().filter_map(|s| s.class.as_deref()) {
        *errors.entry(class.to_owned()).or_insert(0) += 1;
    }
    let ok = window.iter().filter(|s| s.class.is_none()).count();
    #[allow(clippy::cast_precision_loss)] // window sizes are at most 500
    let success_rate = if window.is_empty() {
        1.0
    } else {
        ok as f64 / window.len() as f64
    };
    let mut ttfts: Vec<u64> = window.iter().filter_map(|s| s.ttft_ms).collect();
    ttfts.sort_unstable();
    let mut tps: Vec<f64> = window.iter().filter_map(|s| s.output_tps).collect();
    tps.sort_by(f64::total_cmp);
    let ttft_p50_ms = percentile(&ttfts, 50).copied();
    let last_call = window.first().map(|s| s.ts);
    let raw = raw_score(success_rate, ttft_p50_ms);
    Record {
        provider: key.0.to_owned(),
        model: key.1.to_owned(),
        credential: key.2.to_owned(),
        calls: window.len(),
        success_rate,
        errors,
        ttft_p50_ms,
        ttft_p95_ms: percentile(&ttfts, 95).copied(),
        output_tps_p50: percentile(&tps, 50).copied(),
        last_call: last_call.map(|t| t.to_string()),
        score: decayed(raw, last_call, settings.half_life, now),
    }
}

/// The nearest-rank `p`th percentile of sorted `values`.
fn percentile<T>(values: &[T], p: usize) -> Option<&T> {
    if values.is_empty() {
        return None;
    }
    let rank = (p * values.len()).div_ceil(100).max(1);
    values.get(rank - 1)
}

#[allow(clippy::cast_precision_loss)] // milliseconds far below 2^52
fn raw_score(success_rate: f64, ttft_p50_ms: Option<u64>) -> f64 {
    let speed = ttft_p50_ms.map_or(1.0, |ms| {
        (1.0 - (ms as f64 - FAST).max(0.0) / (SLOW - FAST)).clamp(0.0, 1.0)
    });
    (SUCCESS_WEIGHT * success_rate + (1.0 - SUCCESS_WEIGHT) * speed).clamp(0.0, 1.0)
}

/// `raw` decayed toward 1.0 over the time since `last`.
fn decayed(raw: f64, last: Option<Timestamp>, half_life: Duration, now: Timestamp) -> f64 {
    let Some(last) = last else { return 1.0 };
    let idle = now.duration_since(last).as_secs_f64().max(0.0);
    let remaining = 0.5_f64.powf(idle / half_life.as_secs_f64());
    1.0 - (1.0 - raw) * remaining
}

/// Health scores kept in the profile's `state.db`: records every sent
/// call (as the caller's [`CallObserver`]) and answers fallback walks
/// (as their [`RouteHealth`], with the credential pool's benches).
pub struct StoreHealth {
    store: Arc<Store>,
    settings: HealthSettings,
    pool: Option<Arc<dyn PoolState>>,
}

impl std::fmt::Debug for StoreHealth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoreHealth")
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

impl StoreHealth {
    /// Health in `store` under `config`'s `[routing]`.
    #[must_use]
    pub fn new(store: Arc<Store>, config: &Config) -> Self {
        Self {
            store,
            settings: HealthSettings::of(config),
            pool: None,
        }
    }

    /// Also answers benches from the credential pool's `state`.
    #[must_use]
    pub fn with_pool(mut self, state: Arc<dyn PoolState>) -> Self {
        self.pool = Some(state);
        self
    }

    /// Every record, by provider, model and credential.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn records(&self, now: Timestamp) -> Result<Vec<Record>, Error> {
        let mut records = Vec::new();
        for (provider, model, credential) in self.store.provider_call_keys()? {
            let samples = self.store.provider_calls(
                &provider,
                &model,
                Some(&credential),
                self.settings.window,
            )?;
            records.push(record(
                (&provider, &model, &credential),
                &samples,
                self.settings,
                now,
            ));
        }
        Ok(records)
    }
}

impl CallObserver for StoreHealth {
    fn observed(&self, route: &Route, outcome: &Observed) {
        let sample = CallSample {
            provider: route.provider.clone(),
            model: route.model.clone(),
            credential: route
                .credential
                .state_id()
                .unwrap_or_else(|| "none".to_owned()),
            ts: Timestamp::now(),
            class: outcome.class.map(|c| c.as_str().to_owned()),
            ttft_ms: outcome
                .ttft
                .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
            output_tps: outcome.output_tps,
        };
        if let Err(err) = self.store.record_provider_call(&sample) {
            tracing::warn!(error = %err, "cannot record a provider health sample");
        }
    }
}

impl RouteHealth for StoreHealth {
    fn benched_for(&self, provider: &str, now: Timestamp) -> Option<Duration> {
        let states = self.pool.as_ref()?.states(provider).ok()?;
        if states.is_empty() {
            return None;
        }
        let mut soonest: Option<Timestamp> = None;
        for state in &states {
            match state.benched_until.filter(|until| *until > now) {
                Some(until) => soonest = Some(soonest.map_or(until, |s| s.min(until))),
                None => return None,
            }
        }
        soonest.map(|until| until.duration_since(now).unsigned_abs())
    }

    fn score(&self, provider: &str, model: &str) -> f64 {
        match self
            .store
            .provider_calls(provider, model, None, self.settings.window)
        {
            Ok(samples) => {
                record(
                    (provider, model, ""),
                    &samples,
                    self.settings,
                    Timestamp::now(),
                )
                .score
            }
            Err(err) => {
                tracing::warn!(error = %err, "cannot read provider health");
                1.0
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failed(ts: Timestamp, class: &str) -> CallSample {
        CallSample {
            provider: "p".to_owned(),
            model: "m".to_owned(),
            credential: "c".to_owned(),
            ts,
            class: Some(class.to_owned()),
            ttft_ms: None,
            output_tps: None,
        }
    }

    const DEFAULTS: HealthSettings = HealthSettings {
        window: 50,
        half_life: Duration::from_mins(30),
    };

    #[test]
    fn percentiles_use_the_nearest_rank() {
        let values: Vec<u64> = (1..=20).collect();
        assert_eq!(percentile(&values, 50), Some(&10));
        assert_eq!(percentile(&values, 95), Some(&19));
        assert_eq!(percentile::<u64>(&[], 50), None);
    }

    #[test]
    fn a_slow_but_healthy_route_scores_below_a_fast_one() {
        assert!((raw_score(1.0, Some(500)) - 1.0).abs() < 1e-9);
        assert!(raw_score(1.0, Some(40_000)) < 0.86);
        assert!((raw_score(1.0, None) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn the_window_bounds_what_counts() {
        let now = Timestamp::now();
        let samples: Vec<CallSample> = (0..5).map(|_| failed(now, "overloaded")).collect();
        let settings = HealthSettings {
            window: 3,
            ..DEFAULTS
        };
        let rec = record(("p", "m", "c"), &samples, settings, now);
        assert_eq!(rec.calls, 3);
        assert_eq!(rec.errors["overloaded"], 3);
    }
}

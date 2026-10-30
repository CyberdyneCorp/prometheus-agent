//! Credential pools and benches (spec: model-providers, Credential pool
//! selection strategies; Exhausted credential cooldowns).
//!
//! A provider's pool is its API keys: the first secret name (label
//! `default`) and the labelled keys `<FIRST>_<LABEL>` that `prometheus
//! auth add --label` stores. With two or more a request takes one by
//! `providers.<id>.pool_strategy`. The provider's other secret names are
//! alternatives in preference order (a shared key next to a plan key),
//! used only when no pool key is usable.
//! Every credential (keys and sign-ins) has a request count and an
//! optional bench in [`PoolState`] (`state.db` in production), shared by
//! every process of the profile.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

use jiff::{SignedDuration, Timestamp};
use pmt_config::schema::PoolStrategy;
use pmt_core::error::Error;
use pmt_store::credential_state::CredentialState;

use crate::auth::DEFAULT_LABEL;
use crate::classify::{ErrorClass, ProviderError};
use crate::registry::Provider;

/// Bench after HTTP 401.
pub const AUTH_BENCH: Duration = Duration::from_mins(5);
/// Bench after HTTP 429, HTTP 402 or a billing failure.
pub const EXHAUSTED_BENCH: Duration = Duration::from_hours(1);
/// Longest bench of a provider's only credential (unless billing).
pub const SOLE_BENCH: Duration = Duration::from_mins(1);

/// Request counts and benches of credentials, by provider and credential
/// id ([`secret_id`], [`subscription_id`], [`borrowed_id`]).
pub trait PoolState: Send + Sync {
    /// Every recorded credential of `provider`.
    ///
    /// # Errors
    ///
    /// Store errors.
    fn states(&self, provider: &str) -> Result<Vec<CredentialState>, Error>;

    /// Counts one request served by `credential`.
    ///
    /// # Errors
    ///
    /// Store errors.
    fn record_use(&self, provider: &str, credential: &str, now: Timestamp) -> Result<(), Error>;

    /// Benches `credential` until `until` (`None` clears it).
    ///
    /// # Errors
    ///
    /// Store errors.
    fn bench(
        &self,
        provider: &str,
        credential: &str,
        until: Option<Timestamp>,
        reason: Option<&str>,
    ) -> Result<(), Error>;
}

impl PoolState for pmt_store::Store {
    fn states(&self, provider: &str) -> Result<Vec<CredentialState>, Error> {
        self.credential_states(provider)
    }

    fn record_use(&self, provider: &str, credential: &str, now: Timestamp) -> Result<(), Error> {
        self.record_credential_use(provider, credential, now)
    }

    fn bench(
        &self,
        provider: &str,
        credential: &str,
        until: Option<Timestamp>,
        reason: Option<&str>,
    ) -> Result<(), Error> {
        self.bench_credential(provider, credential, until, reason)
    }
}

/// Pool state held in memory (tests, and callers without a store).
#[derive(Debug, Default)]
pub struct MemoryState {
    rows: Mutex<BTreeMap<(String, String), CredentialState>>,
}

impl MemoryState {
    fn with_row<T>(
        &self,
        provider: &str,
        credential: &str,
        f: impl FnOnce(&mut CredentialState) -> T,
    ) -> T {
        let mut rows = self
            .rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let row = rows
            .entry((provider.to_owned(), credential.to_owned()))
            .or_insert_with(|| CredentialState {
                credential: credential.to_owned(),
                request_count: 0,
                last_used_at: None,
                benched_until: None,
                bench_reason: None,
            });
        f(row)
    }
}

impl PoolState for MemoryState {
    fn states(&self, provider: &str) -> Result<Vec<CredentialState>, Error> {
        let rows = self
            .rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(rows
            .iter()
            .filter(|((p, _), _)| p == provider)
            .map(|(_, row)| row.clone())
            .collect())
    }

    fn record_use(&self, provider: &str, credential: &str, now: Timestamp) -> Result<(), Error> {
        self.with_row(provider, credential, |row| {
            row.request_count += 1;
            row.last_used_at = Some(now);
        });
        Ok(())
    }

    fn bench(
        &self,
        provider: &str,
        credential: &str,
        until: Option<Timestamp>,
        reason: Option<&str>,
    ) -> Result<(), Error> {
        self.with_row(provider, credential, |row| {
            row.benched_until = until;
            row.bench_reason = reason.map(str::to_owned);
        });
        Ok(())
    }
}

/// The state id of the API key held in secret `name`.
#[must_use]
pub fn secret_id(name: &str) -> String {
    format!("secret:{name}")
}

/// The state id of sign-in `label`.
#[must_use]
pub fn subscription_id(label: &str) -> String {
    format!("subscription:{label}")
}

/// The state id of the login borrowed from `cli`.
#[must_use]
pub fn borrowed_id(cli: &str) -> String {
    format!("borrowed:{cli}")
}

/// One API key of a provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    /// `default`, a lower-case secret name, or the `auth add --label`.
    pub label: String,
    /// The secret holding it.
    pub secret: String,
    /// In the pool (the first secret name or a labelled key) rather than
    /// an alternative secret name.
    pub pooled: bool,
}

/// The provider's API keys among the set secret `names` in listing order:
/// the first secret name, the provider's other secret names, then
/// labelled keys by name.
#[must_use]
pub fn key_members(provider: &Provider, names: &[String]) -> Vec<Member> {
    let own = provider.secret_names();
    let Some(first) = own.first() else {
        return Vec::new();
    };
    let set = |name: &String| names.contains(name);
    let mut members: Vec<Member> = own
        .iter()
        .enumerate()
        .filter(|(_, name)| set(name))
        .map(|(i, name)| Member {
            label: if i == 0 {
                DEFAULT_LABEL.to_owned()
            } else {
                label_of(name)
            },
            secret: name.clone(),
            pooled: i == 0,
        })
        .collect();
    let prefix = format!("{first}_");
    let mut labelled: Vec<&String> = names
        .iter()
        .filter(|n| n.starts_with(&prefix) && !own.contains(n))
        .collect();
    labelled.sort();
    members.extend(labelled.into_iter().map(|name| Member {
        label: label_of(&name[prefix.len()..]),
        secret: name.clone(),
        pooled: true,
    }));
    members
}

fn label_of(name: &str) -> String {
    name.to_ascii_lowercase().replace('_', "-")
}

/// When `id` is benched past `now`.
#[must_use]
pub fn benched_until(states: &[CredentialState], id: &str, now: Timestamp) -> Option<Timestamp> {
    states
        .iter()
        .find(|s| s.credential == id)
        .and_then(|s| s.benched_until)
        .filter(|until| *until > now)
}

/// Picks one of `ids` (priority order) by `strategy`, skipping benched
/// ones; `None` when every one is benched. `round_robin` takes the
/// credential that served least recently (never-used first), so a fixed
/// pool rotates A, B, A, B across processes; `least_used` the lowest
/// request count; ties go to priority order.
#[must_use]
pub fn select(
    strategy: PoolStrategy,
    ids: &[String],
    states: &[CredentialState],
    now: Timestamp,
) -> Option<usize> {
    let state = |id: &String| states.iter().find(|s| &s.credential == id);
    let usable: Vec<usize> = (0..ids.len())
        .filter(|&i| benched_until(states, &ids[i], now).is_none())
        .collect();
    match strategy {
        PoolStrategy::FillFirst => usable.first().copied(),
        PoolStrategy::RoundRobin => usable
            .iter()
            .copied()
            .min_by_key(|&i| state(&ids[i]).and_then(|s| s.last_used_at)),
        PoolStrategy::LeastUsed => usable
            .iter()
            .copied()
            .min_by_key(|&i| state(&ids[i]).map_or(0, |s| s.request_count)),
        PoolStrategy::Random => {
            (!usable.is_empty()).then(|| usable[fastrand::usize(..usable.len())])
        }
    }
}

/// How long a failure benches the credential that served it: until the
/// provider's reset time when it is in the future, else 300 s after HTTP
/// 401 and 3600 s after HTTP 429, HTTP 402 or a billing failure; at most
/// 60 s when `sole` (the provider's only usable credential) unless the
/// failure is billing. `None`: the failure does not bench.
#[must_use]
pub fn cooldown(
    error: &ProviderError,
    sole: bool,
    now: Timestamp,
) -> Option<(Timestamp, &'static str)> {
    let billing = error.status == Some(402) || error.class == ErrorClass::Billing;
    let (default, reason) = match error.status {
        _ if billing => (EXHAUSTED_BENCH, "billing"),
        Some(401) => (AUTH_BENCH, "auth"),
        Some(429) => (EXHAUSTED_BENCH, "rate_limit"),
        _ if error.class == ErrorClass::RateLimit => (EXHAUSTED_BENCH, "rate_limit"),
        _ => return None,
    };
    let reset = error
        .reset_at
        .or_else(|| {
            error
                .retry_after
                .and_then(|d| now.checked_add(SignedDuration::try_from(d).ok()?).ok())
        })
        .filter(|at| *at > now);
    let mut until = reset.unwrap_or_else(|| later(now, default));
    if sole && !billing {
        until = until.min(later(now, SOLE_BENCH));
    }
    Some((until, reason))
}

/// `now + by`, saturating.
#[must_use]
pub fn later(now: Timestamp, by: Duration) -> Timestamp {
    SignedDuration::try_from(by)
        .ok()
        .and_then(|d| now.checked_add(d).ok())
        .unwrap_or(Timestamp::MAX)
}

#[cfg(test)]
mod tests;

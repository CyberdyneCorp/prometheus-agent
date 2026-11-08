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

    /// Where API key `credential` of `provider` came from and whether it
    /// is dead (`None`: typed by hand, never dead).
    ///
    /// # Errors
    ///
    /// Store errors.
    fn key_origin(
        &self,
        _provider: &str,
        _credential: &str,
    ) -> Result<Option<(String, bool)>, Error> {
        Ok(None)
    }

    /// Marks minted key `credential` of `provider` dead.
    ///
    /// # Errors
    ///
    /// Store errors.
    fn mark_key_dead(&self, _provider: &str, _credential: &str) -> Result<(), Error> {
        Ok(())
    }

    /// A value the profile remembers under `key` (a detected endpoint).
    ///
    /// # Errors
    ///
    /// Store errors.
    fn remembered(&self, _key: &str) -> Result<Option<String>, Error> {
        Ok(None)
    }

    /// Remembers `value` under `key`.
    ///
    /// # Errors
    ///
    /// Store errors.
    fn remember(&self, _key: &str, _value: &str) -> Result<(), Error> {
        Ok(())
    }
}

impl PoolState for pmt_store::Store {
    fn states(&self, provider: &str) -> Result<Vec<CredentialState>, Error> {
        self.credential_states(provider)
    }

    fn key_origin(
        &self,
        provider: &str,
        credential: &str,
    ) -> Result<Option<(String, bool)>, Error> {
        pmt_store::Store::key_origin(self, provider, credential)
    }

    fn mark_key_dead(&self, provider: &str, credential: &str) -> Result<(), Error> {
        pmt_store::Store::mark_key_dead(self, provider, credential)
    }

    fn remembered(&self, key: &str) -> Result<Option<String>, Error> {
        self.pref(&format!("providers.{key}"))
    }

    fn remember(&self, key: &str, value: &str) -> Result<(), Error> {
        self.set_pref(&format!("providers.{key}"), value)
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
    memo: Mutex<BTreeMap<String, String>>,
}

impl MemoryState {
    /// Runs `f` on the row of `credential` with the provider's highest use
    /// sequence, under one lock.
    fn with_row<T>(
        &self,
        provider: &str,
        credential: &str,
        f: impl FnOnce(&mut CredentialState, u64) -> T,
    ) -> T {
        let mut rows = self
            .rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let last_seq = rows
            .iter()
            .filter(|((p, _), _)| p == provider)
            .filter_map(|(_, row)| row.use_seq)
            .max()
            .unwrap_or(0);
        let row = rows
            .entry((provider.to_owned(), credential.to_owned()))
            .or_insert_with(|| CredentialState {
                credential: credential.to_owned(),
                request_count: 0,
                last_used_at: None,
                use_seq: None,
                benched_until: None,
                bench_reason: None,
            });
        f(row, last_seq)
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
        self.with_row(provider, credential, |row, last_seq| {
            row.request_count += 1;
            row.last_used_at = Some(now);
            row.use_seq = Some(last_seq + 1);
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
        self.with_row(provider, credential, |row, _| {
            row.benched_until = until;
            row.bench_reason = reason.map(str::to_owned);
        });
        Ok(())
    }

    fn remembered(&self, key: &str) -> Result<Option<String>, Error> {
        let memo = self
            .memo
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(memo.get(key).cloned())
    }

    fn remember(&self, key: &str, value: &str) -> Result<(), Error> {
        let mut memo = self
            .memo
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        memo.insert(key.to_owned(), value.to_owned());
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
    /// `default`, a lower-case secret name (`-<n>` for a numbered
    /// sibling), or the `auth add --label`.
    pub label: String,
    /// The secret holding it.
    pub secret: String,
    /// Seeded from the provider's secret names (or a numbered sibling
    /// `<NAME>_<n>`) rather than stored by `prometheus auth add --label`:
    /// it leaves the pool when its secret disappears.
    pub seeded: bool,
    /// Rotated in the pool: the first secret name that is set, its
    /// numbered siblings and the labelled keys. The provider's later
    /// secret names (a shared key after a plan key) and their siblings are
    /// alternatives, used in order only when no pooled key is usable, so
    /// the first name present supplies the credential.
    pub pooled: bool,
}

/// The provider's keys among the set secret `names`, in order (spec:
/// model-providers, Credential pool seeding, custom endpoint pools and
/// child sharing): each of its secret names that is set, followed by its
/// numbered siblings `<NAME>_2`, `<NAME>_3`... (all `seeded`), then the
/// labelled keys `<FIRST>_<LABEL>` by name. See [`Member::pooled`] for
/// which of them rotate. Membership follows the set
/// secrets, so a seeded member whose secret disappears is pruned, while a
/// labelled key stays until `auth remove`.
#[must_use]
pub fn key_members(provider: &Provider, names: &[String]) -> Vec<Member> {
    let own = provider.secret_names();
    let Some(first) = own.first() else {
        return Vec::new();
    };
    let mut members = Vec::new();
    let mut pool_group = None;
    for (i, name) in own.iter().enumerate() {
        let group = seeded_group(i, name, &own, names);
        if !group.is_empty() && pool_group.is_none() {
            pool_group = Some(i);
        }
        let pooled = pool_group == Some(i);
        members.extend(group.into_iter().map(|(label, secret)| Member {
            label,
            secret: secret.clone(),
            seeded: true,
            pooled,
        }));
    }
    let prefix = format!("{first}_");
    let mut labelled: Vec<&String> = names
        .iter()
        .filter(|n| n.starts_with(&prefix) && !own.contains(n))
        .filter(|n| sibling_number(&n[prefix.len()..]).is_none())
        .collect();
    labelled.sort();
    members.extend(labelled.into_iter().map(|name| Member {
        label: label_of(&name[prefix.len()..]),
        secret: name.clone(),
        seeded: false,
        pooled: true,
    }));
    members
}

/// The set keys of secret name `i` of `own`: itself, then its numbered
/// siblings, labelled `default`/`<n>` for the first name and
/// `<name>`/`<name>-<n>` for the others.
fn seeded_group<'a>(
    i: usize,
    name: &str,
    own: &[String],
    names: &'a [String],
) -> Vec<(String, &'a String)> {
    let base = if i == 0 {
        DEFAULT_LABEL.to_owned()
    } else {
        label_of(name)
    };
    let mut group: Vec<(String, &String)> = names
        .iter()
        .filter(|n| *n == name)
        .map(|n| (base.clone(), n))
        .collect();
    for (n, sibling) in siblings(name, names) {
        if own.contains(sibling) {
            continue;
        }
        let label = if i == 0 {
            n.to_string()
        } else {
            format!("{base}-{n}")
        };
        group.push((label, sibling));
    }
    group
}

/// The numbered siblings `<name>_<n>` (n >= 2) among `names`, by number.
fn siblings<'a>(name: &str, names: &'a [String]) -> Vec<(u32, &'a String)> {
    let prefix = format!("{name}_");
    let mut found: Vec<(u32, &String)> = names
        .iter()
        .filter_map(|n| Some((sibling_number(n.strip_prefix(&prefix)?)?, n)))
        .collect();
    found.sort();
    found
}

/// `n` of a sibling suffix: digits only, 2 or more, no leading zero.
fn sibling_number(suffix: &str) -> Option<u32> {
    let all_digits = !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit());
    if !all_digits || suffix.starts_with('0') {
        return None;
    }
    suffix.parse().ok().filter(|n| *n >= 2)
}

fn label_of(name: &str) -> String {
    name.to_ascii_lowercase().replace('_', "-")
}

/// The profile-state key of `provider`'s pool order.
fn priority_key(provider: &str) -> String {
    format!("priority.{provider}")
}

/// `provider`'s pool order set by `prometheus auth priority`: credential
/// ids first to last (`fill_first` takes the first usable one). Empty
/// when none was set or it cannot be read.
#[must_use]
pub fn priority(state: Option<&dyn PoolState>, provider: &str) -> Vec<String> {
    state
        .and_then(|s| s.remembered(&priority_key(provider)).ok().flatten())
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Stores `provider`'s pool order.
///
/// # Errors
///
/// Store errors.
///
/// # Panics
///
/// Never: a list of strings always serializes.
pub fn set_priority(state: &dyn PoolState, provider: &str, ids: &[String]) -> Result<(), Error> {
    let text = serde_json::to_string(ids).expect("strings serialize");
    state.remember(&priority_key(provider), &text)
}

/// Orders `items` by `priority` (ids listed there first, in its order),
/// keeping the original order of the rest.
pub fn prioritize<T>(items: &mut [T], priority: &[String], id: impl Fn(&T) -> &str) {
    items.sort_by_key(|item| {
        priority
            .iter()
            .position(|p| p == id(item))
            .unwrap_or(usize::MAX)
    });
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
/// pool rotates A, B, A, B across processes; recency is the use sequence,
/// not `last_used_at`, whose milliseconds tie under a burst (rows not used
/// since schema v7 have no sequence and order by time). `least_used` takes
/// the lowest request count; ties go to priority order.
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
            .min_by_key(|&i| state(&ids[i]).map(|s| (s.use_seq, s.last_used_at))),
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

//! Availability probes, cached per profile with flake grace (spec:
//! tool-system, Availability probes are cached per profile with flake
//! grace).
//!
//! A probe answers only reachability or opt-in ("is the container daemon
//! up", "is a browser runtime installed", "is a search provider
//! configured"), never anything about one session, so its verdict can be
//! shared by every session of the profile:
//!
//! - a verdict is cached for [`CACHE_TTL`] keyed by (probe id, profile);
//! - a probe that errors or runs longer than [`PROBE_TIMEOUT`] fails;
//! - a failure within [`FLAKE_GRACE`] of the probe's last success counts as
//!   available and is not cached, so the next build probes again;
//! - otherwise the failure is cached like a success.
//!
//! Time is tokio's clock, so tests drive it with `tokio::time::pause`.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use pmt_config::schema::Config;
use tokio::time::Instant;

/// How long a verdict is reused.
pub const CACHE_TTL: Duration = Duration::from_secs(30);
/// A probe slower than this fails.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// A failure this soon after a success is treated as a flake.
pub const FLAKE_GRACE: Duration = Duration::from_mins(1);

/// What a probe may look at: the profile it answers for and its config.
#[derive(Debug, Clone, Copy)]
pub struct ProbeContext<'a> {
    /// Profile name (the cache key).
    pub profile: &'a str,
    /// The profile's configuration.
    pub config: &'a Config,
}

/// A probe's answer: `Err` carries the reason shown as
/// `unavailable:<reason>`.
pub type ProbeFuture<'a> = Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

/// A reachability or opt-in check shared by the tools that depend on it.
pub trait Probe: Send + Sync {
    /// Stable id; tools sharing a probe share its verdict.
    fn id(&self) -> &str;
    /// Checks availability for a profile.
    fn check<'a>(&'a self, context: ProbeContext<'a>) -> ProbeFuture<'a>;
}

/// A probe from a closure (tests and simple opt-in checks).
pub struct FnProbe<F> {
    id: String,
    check: F,
}

impl<F> FnProbe<F>
where
    F: Fn(ProbeContext<'_>) -> Result<(), String> + Send + Sync,
{
    /// A probe named `id` answering with `check`.
    pub fn new(id: impl Into<String>, check: F) -> Self {
        Self {
            id: id.into(),
            check,
        }
    }
}

impl<F> Probe for FnProbe<F>
where
    F: Fn(ProbeContext<'_>) -> Result<(), String> + Send + Sync,
{
    fn id(&self) -> &str {
        &self.id
    }

    fn check<'a>(&'a self, context: ProbeContext<'a>) -> ProbeFuture<'a> {
        let verdict = (self.check)(context);
        Box::pin(async move { verdict })
    }
}

#[derive(Debug, Default)]
struct Entry {
    /// The cached verdict and when it was taken.
    cached: Option<(Result<(), String>, Instant)>,
    last_success: Option<Instant>,
}

/// Verdicts shared by every surface build of the process.
#[derive(Debug, Clone, Default)]
pub struct ProbeCache {
    entries: Arc<Mutex<HashMap<(String, String), Entry>>>,
}

impl ProbeCache {
    /// An empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The verdict of `probe` for `context.profile`: cached, or fresh.
    ///
    /// # Errors
    ///
    /// The reason the tools depending on `probe` are unavailable.
    pub async fn verdict(
        &self,
        probe: &dyn Probe,
        context: ProbeContext<'_>,
    ) -> Result<(), String> {
        let key = (probe.id().to_owned(), context.profile.to_owned());
        if let Some(verdict) = self.cached(&key) {
            return verdict;
        }
        let outcome = match tokio::time::timeout(PROBE_TIMEOUT, probe.check(context)).await {
            Ok(outcome) => outcome,
            Err(_) => Err(format!(
                "probe timed out after {}s",
                PROBE_TIMEOUT.as_secs()
            )),
        };
        self.record(key, outcome)
    }

    fn cached(&self, key: &(String, String)) -> Option<Result<(), String>> {
        let entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let (verdict, at) = entries.get(key)?.cached.as_ref()?;
        (at.elapsed() < CACHE_TTL).then(|| verdict.clone())
    }

    fn record(&self, key: (String, String), outcome: Result<(), String>) -> Result<(), String> {
        let now = Instant::now();
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = entries.entry(key).or_default();
        if outcome.is_ok() {
            entry.last_success = Some(now);
        } else if entry
            .last_success
            .is_some_and(|success| now.duration_since(success) <= FLAKE_GRACE)
        {
            entry.cached = None;
            return Ok(());
        }
        entry.cached = Some((outcome.clone(), now));
        outcome
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    use pmt_config::schema::Config;

    use super::{Probe, ProbeCache, ProbeContext, ProbeFuture};

    /// A probe whose answer the test flips, counting its runs.
    struct Switch {
        up: AtomicBool,
        runs: AtomicUsize,
        delay: Duration,
    }

    impl Switch {
        fn new(up: bool) -> Arc<Self> {
            Arc::new(Self {
                up: AtomicBool::new(up),
                runs: AtomicUsize::new(0),
                delay: Duration::ZERO,
            })
        }
        fn set(&self, up: bool) {
            self.up.store(up, Ordering::SeqCst);
        }
        fn runs(&self) -> usize {
            self.runs.load(Ordering::SeqCst)
        }
    }

    impl Probe for Switch {
        fn id(&self) -> &'static str {
            "container"
        }
        fn check<'a>(&'a self, _: ProbeContext<'a>) -> ProbeFuture<'a> {
            Box::pin(async move {
                self.runs.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(self.delay).await;
                if self.up.load(Ordering::SeqCst) {
                    Ok(())
                } else {
                    Err("daemon not reachable".into())
                }
            })
        }
    }

    fn context<'a>(profile: &'a str, config: &'a Config) -> ProbeContext<'a> {
        ProbeContext { profile, config }
    }

    async fn advance(secs: u64) {
        tokio::time::advance(Duration::from_secs(secs)).await;
    }

    #[tokio::test(start_paused = true)]
    async fn verdicts_are_cached_for_thirty_seconds_per_profile() {
        let (cache, probe, config) = (ProbeCache::new(), Switch::new(true), Config::default());
        assert_eq!(
            cache
                .verdict(probe.as_ref(), context("work", &config))
                .await,
            Ok(())
        );
        assert_eq!(
            cache
                .verdict(probe.as_ref(), context("work", &config))
                .await,
            Ok(())
        );
        assert_eq!(probe.runs(), 1);
        cache
            .verdict(probe.as_ref(), context("home", &config))
            .await
            .unwrap();
        assert_eq!(probe.runs(), 2, "another profile has its own entry");
        advance(31).await;
        cache
            .verdict(probe.as_ref(), context("work", &config))
            .await
            .unwrap();
        assert_eq!(probe.runs(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn transient_failure_after_a_recent_success_stays_available() {
        let (cache, probe, config) = (ProbeCache::new(), Switch::new(true), Config::default());
        cache
            .verdict(probe.as_ref(), context("p", &config))
            .await
            .unwrap();
        advance(31).await;
        probe.set(false);
        // Last success 31 s ago (< 60 s): available, failure not cached.
        assert_eq!(
            cache.verdict(probe.as_ref(), context("p", &config)).await,
            Ok(())
        );
        assert_eq!(
            cache.verdict(probe.as_ref(), context("p", &config)).await,
            Ok(())
        );
        assert_eq!(probe.runs(), 3, "the next build re-probes");
    }

    #[tokio::test(start_paused = true)]
    async fn persistent_outage_is_cached_as_unavailable() {
        let (cache, probe, config) = (ProbeCache::new(), Switch::new(true), Config::default());
        cache
            .verdict(probe.as_ref(), context("p", &config))
            .await
            .unwrap();
        advance(61).await;
        probe.set(false);
        let reason = Err("daemon not reachable".to_owned());
        assert_eq!(
            cache.verdict(probe.as_ref(), context("p", &config)).await,
            reason
        );
        probe.set(true);
        advance(29).await;
        assert_eq!(
            cache.verdict(probe.as_ref(), context("p", &config)).await,
            reason,
            "the failure is cached for 30 s"
        );
        assert_eq!(probe.runs(), 2);
        advance(2).await;
        assert_eq!(
            cache.verdict(probe.as_ref(), context("p", &config)).await,
            Ok(())
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_probe_is_unavailable() {
        let probe = Arc::new(Switch {
            up: AtomicBool::new(true),
            runs: AtomicUsize::new(0),
            delay: Duration::from_secs(6),
        });
        let (cache, config) = (ProbeCache::new(), Config::default());
        assert_eq!(
            cache.verdict(probe.as_ref(), context("p", &config)).await,
            Err("probe timed out after 5s".to_owned())
        );
    }
}

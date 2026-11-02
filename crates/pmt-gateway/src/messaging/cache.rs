//! Live agents kept in memory (spec: messaging-gateway, Agent cache and
//! eviction; Conversations never rotate on time): the conversations whose
//! agent is loaded form an LRU of at most `gateway.agent_cache.max_size`;
//! an agent idle for `idle_ttl_secs`, the least recently used beyond the
//! size, and the least recently used half while the daemon's resident
//! memory exceeds `memory_high_bytes` (`0`: half the physical memory) are
//! unloaded. Unloading never ends the session and is refused by the host
//! while a turn runs; the next message rebuilds the agent from the store.
//! The same sweep prunes stale routes at most once an hour.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use jiff::{SignedDuration, Timestamp};
use pmt_config::schema::AgentCacheConfig;
use tokio::time::Instant;
use tracing::Instrument as _;

use super::Shared;

/// How often the cache is swept at most and at least.
const SWEEP_MIN: Duration = Duration::from_secs(1);
const SWEEP_MAX: Duration = Duration::from_mins(1);
/// How often routes are pruned at most.
const PRUNE_EVERY: Duration = Duration::from_hours(1);

#[derive(Debug, Clone)]
struct Entry {
    profile: String,
    last_used: Instant,
}

/// The loaded conversations, by session.
#[derive(Debug)]
pub(crate) struct AgentCache {
    config: AgentCacheConfig,
    entries: Mutex<HashMap<String, Entry>>,
}

impl AgentCache {
    pub(crate) fn new(config: AgentCacheConfig) -> Self {
        Self {
            config,
            entries: Mutex::default(),
        }
    }

    /// The session's agent was used now.
    pub(crate) fn touch(&self, profile: &str, session: &str) {
        self.entries.lock().expect("cache poisoned").insert(
            session.to_owned(),
            Entry {
                profile: profile.to_owned(),
                last_used: Instant::now(),
            },
        );
    }

    fn forget(&self, session: &str) {
        self.entries.lock().expect("cache poisoned").remove(session);
    }

    /// Sessions to unload at `now`, least recently used first: the idle
    /// ones, those beyond the size, and with `over_memory` half of the rest.
    pub(crate) fn victims(&self, now: Instant, over_memory: bool) -> Vec<(String, String)> {
        let entries = self.entries.lock().expect("cache poisoned");
        let mut by_age: Vec<(&String, &Entry)> = entries.iter().collect();
        by_age.sort_by_key(|(_, e)| e.last_used);
        let ttl = Duration::from_secs(self.config.idle_ttl_secs);
        let max = usize::try_from(self.config.max_size).unwrap_or(usize::MAX);
        let excess = by_age.len().saturating_sub(max);
        let half = if over_memory {
            by_age.len().div_ceil(2)
        } else {
            0
        };
        by_age
            .iter()
            .enumerate()
            .filter(|(rank, (_, entry))| {
                *rank < excess.max(half) || now.duration_since(entry.last_used) >= ttl
            })
            .map(|(_, (session, entry))| (entry.profile.clone(), (*session).clone()))
            .collect()
    }

    /// A quarter of the idle TTL, within one second and one minute, so an
    /// idle agent outlives its TTL by little.
    fn sweep_every(&self) -> Duration {
        (Duration::from_secs(self.config.idle_ttl_secs) / 4).clamp(SWEEP_MIN, SWEEP_MAX)
    }

    fn memory_high(&self) -> u64 {
        if self.config.memory_high_bytes > 0 {
            return self.config.memory_high_bytes;
        }
        let mut system = sysinfo::System::new();
        system.refresh_memory();
        system.total_memory() / 2
    }
}

/// The daemon's resident memory in bytes.
fn resident_bytes() -> Option<u64> {
    let pid = sysinfo::get_current_pid().ok()?;
    let mut system = sysinfo::System::new();
    system.refresh_processes_specifics(
        sysinfo::ProcessesToUpdate::Some(&[pid]),
        true,
        sysinfo::ProcessRefreshKind::nothing().with_memory(),
    );
    system.process(pid).map(sysinfo::Process::memory)
}

/// Starts the sweeper.
pub(crate) fn spawn(shared: &Arc<Shared>) {
    let span = tracing::info_span!("agent_cache");
    tokio::spawn(run(Arc::clone(shared)).instrument(span));
}

async fn run(shared: Arc<Shared>) {
    let mut pruned_at: Option<Instant> = None;
    loop {
        tokio::select! {
            () = shared.stop.cancelled() => return,
            () = tokio::time::sleep(shared.cache.sweep_every()) => {}
        }
        sweep(&shared).await;
        if pruned_at.is_none_or(|at| at.elapsed() >= PRUNE_EVERY) {
            pruned_at = Some(Instant::now());
            prune_routes(&shared);
            shared.limits.sweep(tokio::time::Instant::now());
        }
    }
}

/// Unloads what the cache no longer keeps.
pub(crate) async fn sweep(shared: &Shared) {
    let cache = &shared.cache;
    let over_memory = resident_bytes().is_some_and(|rss| rss > cache.memory_high());
    for (profile, session) in cache.victims(Instant::now(), over_memory) {
        match shared.agents.evict(&profile, &session).await {
            Ok(true) => {
                tracing::debug!(profile, session_id = session, "agent unloaded");
                cache.forget(&session);
            }
            Ok(false) => {}
            Err(err) => tracing::warn!(profile, session_id = session, "agent not unloaded: {err}"),
        }
    }
}

/// Removes routes unused for `gateway.sessions.route_max_age_secs` (their
/// sessions and transcripts stay).
pub(crate) fn prune_routes(shared: &Shared) {
    for profile in &shared.server.profiles() {
        let (Some(store), Some(scope)) = (shared.store(profile), shared.agents.scope(profile))
        else {
            continue;
        };
        let age = scope.config().gateway.sessions.route_max_age_secs;
        if age == 0 {
            continue;
        }
        let age = SignedDuration::from_secs(i64::try_from(age).unwrap_or(i64::MAX));
        let before = Timestamp::now()
            .saturating_sub(age)
            .unwrap_or(Timestamp::MIN);
        match store.prune_routes(before) {
            Ok(0) => {}
            Ok(n) => tracing::info!(profile, "pruned {n} stale conversation route(s)"),
            Err(err) => tracing::warn!(profile, "route pruning failed: {err}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache(max_size: u32, idle_ttl_secs: u64) -> AgentCache {
        AgentCache::new(AgentCacheConfig {
            max_size,
            idle_ttl_secs,
            memory_high_bytes: 0,
        })
    }

    #[tokio::test(start_paused = true)]
    async fn the_least_recently_used_beyond_the_size_go_first() {
        let cache = cache(2, 3600);
        for s in ["a", "b", "c"] {
            cache.touch("default", s);
            tokio::time::advance(Duration::from_secs(1)).await;
        }
        let victims = cache.victims(Instant::now(), false);
        assert_eq!(victims, vec![("default".to_owned(), "a".to_owned())]);
    }

    #[tokio::test(start_paused = true)]
    async fn idle_agents_and_memory_pressure_unload() {
        let cache = cache(10, 60);
        cache.touch("work", "old");
        tokio::time::advance(Duration::from_secs(61)).await;
        cache.touch("work", "new");
        let idle: Vec<String> = cache
            .victims(Instant::now(), false)
            .into_iter()
            .map(|(_, s)| s)
            .collect();
        assert_eq!(idle, vec!["old"]);
        cache.forget("old");
        tokio::time::advance(Duration::from_secs(1)).await;
        cache.touch("work", "newer");
        let pressured = cache.victims(Instant::now(), true);
        assert_eq!(pressured, vec![("work".to_owned(), "new".to_owned())]);
    }
}

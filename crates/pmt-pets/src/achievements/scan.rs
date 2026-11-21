//! The incremental scan and what surfaces show.
//!
//! Per-session statistics are cached in
//! `<profile>/cache/achievements/sessions.json` under the session id with
//! its `started_at` and `last_active`; a scan analyses only sessions whose
//! key is new or changed and drops deleted ones. Unlocked tiers persist in
//! `<profile>/achievements.json`. A [`Snapshot`] is built from those two
//! files alone, so a page can render at once while a scan runs.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use jiff::Timestamp;
use jiff::civil::{Date, Weekday};
use jiff::tz::TimeZone;
use pmt_core::error::{Error, codes};
use pmt_store::{ListOptions, Store};
use serde::{Deserialize, Serialize};

use super::catalog::{ACHIEVEMENTS, Achievement, Metric, TIERS, tier_of};
use super::stats::{self, SessionStats};

/// The unlock state, under the profile.
pub const UNLOCKS_FILE: &str = "achievements.json";
/// The per-session cache, under the profile.
pub const CACHE_FILE: &str = "cache/achievements/sessions.json";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Cache {
    sessions: BTreeMap<String, Cached>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Cached {
    started_at_ms: i64,
    last_active_ms: i64,
    stats: SessionStats,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Unlocks {
    scanned_at: Option<String>,
    unlocked: BTreeMap<String, Unlocked>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Unlocked {
    tier: String,
    at: String,
}

/// A tier newly reached.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unlock {
    /// The achievement.
    pub id: String,
    /// The tier reached.
    pub tier: String,
}

/// What a scan did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanOutcome {
    /// Sessions analysed (new or changed).
    pub analysed: usize,
    /// Sessions whose cached numbers were reused.
    pub reused: usize,
    /// Tiers reached by this scan (the highest per achievement).
    pub unlocked: Vec<Unlock>,
    /// No scan had run before: the unlocks are past ones, recorded
    /// without a celebration.
    pub first: bool,
}

/// One badge as surfaces show it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Badge {
    /// The achievement id.
    pub id: String,
    /// Its name.
    pub name: String,
    /// Exactly what it counts.
    pub description: String,
    /// Its group.
    pub category: String,
    /// The highest tier unlocked.
    pub tier: Option<String>,
    /// The current count.
    pub value: u64,
    /// The next tier.
    pub next_tier: Option<String>,
    /// The count the next tier needs.
    pub next_threshold: Option<u64>,
    /// When the shown tier unlocked (RFC 3339).
    pub unlocked_at: Option<String>,
}

/// Every badge with the scan's state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    /// The badges.
    pub badges: Vec<Badge>,
    /// Sessions analysed so far.
    pub sessions: usize,
    /// When the last scan finished.
    pub scanned_at: Option<String>,
    /// A scan is running now.
    pub scanning: bool,
}

/// Profiles with a scan running (the snapshot's `scanning`).
static RUNNING: Mutex<Option<HashSet<PathBuf>>> = Mutex::new(None);
/// One scan at a time (a turn's rescan and a page's never interleave).
static SCAN: Mutex<()> = Mutex::new(());

fn mark(dir: &Path, on: bool) {
    let mut running = RUNNING.lock().unwrap_or_else(PoisonError::into_inner);
    let set = running.get_or_insert_with(HashSet::new);
    if on {
        set.insert(dir.to_path_buf());
    } else {
        set.remove(dir);
    }
}

/// Marks a scan of the profile at `dir` as running before it starts on
/// another thread (the snapshot answered meanwhile says so); the scan
/// clears it when it ends.
pub fn mark_running(dir: &Path) {
    mark(dir, true);
}

/// Whether a scan of the profile at `dir` runs now.
#[must_use]
pub fn running(dir: &Path) -> bool {
    RUNNING
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .is_some_and(|s| s.contains(dir))
}

/// One profile's achievements.
#[derive(Debug, Clone)]
pub struct Profile {
    dir: PathBuf,
    zone: TimeZone,
}

fn read_json<T: Default + for<'de> Deserialize<'de>>(path: &Path) -> T {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), Error> {
    let io = |e: std::io::Error| Error::io(&codes::HOME_IO, path, &e);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(io)?;
    }
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(value).unwrap_or_default();
    std::fs::write(&tmp, bytes).map_err(io)?;
    std::fs::rename(&tmp, path).map_err(io)
}

impl Profile {
    /// The achievements of the profile at `dir`, days and hours in
    /// `zone` (the profile's time zone).
    #[must_use]
    pub fn new(dir: &Path, zone: TimeZone) -> Self {
        Self {
            dir: dir.to_path_buf(),
            zone,
        }
    }

    /// Whether a scan ever finished.
    #[must_use]
    pub fn scanned(&self) -> bool {
        self.dir.join(UNLOCKS_FILE).is_file()
    }

    /// Whether no scan finished within `max_age` (or none ever did).
    #[must_use]
    pub fn stale(&self, max_age: std::time::Duration) -> bool {
        let unlocks: Unlocks = read_json(&self.dir.join(UNLOCKS_FILE));
        let last = unlocks
            .scanned_at
            .and_then(|at| at.parse::<Timestamp>().ok());
        last.is_none_or(|at| Timestamp::now().duration_since(at).unsigned_abs() > max_age)
    }

    /// What surfaces show now, from the cache and the unlock state.
    #[must_use]
    pub fn snapshot(&self) -> Snapshot {
        let cache: Cache = read_json(&self.dir.join(CACHE_FILE));
        let unlocks: Unlocks = read_json(&self.dir.join(UNLOCKS_FILE));
        let metrics = Metrics::of(cache.sessions.values().map(|c| &c.stats), &self.zone);
        let badges = ACHIEVEMENTS
            .iter()
            .map(|a| badge(a, metrics.value(a.metric), unlocks.unlocked.get(a.id)))
            .collect();
        Snapshot {
            badges,
            sessions: cache.sessions.len(),
            scanned_at: unlocks.scanned_at,
            scanning: running(&self.dir),
        }
    }

    /// Analyses new and changed sessions of `store`, then records and
    /// returns the tiers newly reached.
    ///
    /// # Errors
    ///
    /// Store read errors; `home.io` writing the files.
    pub fn scan(&self, store: &Store) -> Result<ScanOutcome, Error> {
        let _one = SCAN.lock().unwrap_or_else(PoisonError::into_inner);
        mark(&self.dir, true);
        let outcome = self.scan_locked(store);
        mark(&self.dir, false);
        outcome
    }

    fn scan_locked(&self, store: &Store) -> Result<ScanOutcome, Error> {
        let first = !self.scanned();
        let cache_path = self.dir.join(CACHE_FILE);
        let old: Cache = read_json(&cache_path);
        let mut fresh = Cache::default();
        let mut outcome = ScanOutcome {
            first,
            ..ScanOutcome::default()
        };
        let options = ListOptions {
            all: true,
            ..ListOptions::default()
        };
        for session in store.list_sessions(&options)? {
            let id = session.id.to_string();
            let key = (
                session.started_at.as_millisecond(),
                session.last_activity_at.as_millisecond(),
            );
            let cached = old
                .sessions
                .get(&id)
                .filter(|c| (c.started_at_ms, c.last_active_ms) == key);
            let stats = if let Some(cached) = cached {
                outcome.reused += 1;
                cached.stats.clone()
            } else {
                outcome.analysed += 1;
                stats::analyse(store, &session)?
            };
            fresh.sessions.insert(
                id,
                Cached {
                    started_at_ms: key.0,
                    last_active_ms: key.1,
                    stats,
                },
            );
        }
        write_json(&cache_path, &fresh)?;
        let metrics = Metrics::of(fresh.sessions.values().map(|c| &c.stats), &self.zone);
        outcome.unlocked = self.record(&metrics)?;
        Ok(outcome)
    }

    /// Writes the unlock state; the tiers newly reached.
    fn record(&self, metrics: &Metrics) -> Result<Vec<Unlock>, Error> {
        let path = self.dir.join(UNLOCKS_FILE);
        let mut unlocks: Unlocks = read_json(&path);
        let now = Timestamp::now().to_string();
        let mut reached = Vec::new();
        for achievement in ACHIEVEMENTS {
            let Some(tier) = tier_of(achievement, metrics.value(achievement.metric)) else {
                continue;
            };
            let had = unlocks
                .unlocked
                .get(achievement.id)
                .and_then(|u| TIERS.iter().position(|t| *t == u.tier));
            if had.is_some_and(|had| had >= tier) {
                continue;
            }
            unlocks.unlocked.insert(
                achievement.id.to_owned(),
                Unlocked {
                    tier: TIERS[tier].to_owned(),
                    at: now.clone(),
                },
            );
            reached.push(Unlock {
                id: achievement.id.to_owned(),
                tier: TIERS[tier].to_owned(),
            });
        }
        unlocks.scanned_at = Some(now);
        write_json(&path, &unlocks)?;
        Ok(reached)
    }
}

fn badge(achievement: &Achievement, value: u64, unlocked: Option<&Unlocked>) -> Badge {
    let next = achievement.tiers.iter().position(|&t| value < t);
    Badge {
        id: achievement.id.to_owned(),
        name: achievement.name.to_owned(),
        description: achievement.description.to_owned(),
        category: achievement.category.to_owned(),
        tier: unlocked.map(|u| u.tier.clone()),
        value,
        next_tier: next.map(|i| TIERS[i].to_owned()),
        next_threshold: next.map(|i| achievement.tiers[i]),
        unlocked_at: unlocked.map(|u| u.at.clone()),
    }
}

/// The history's numbers.
#[derive(Debug, Default)]
struct Metrics {
    values: BTreeMap<&'static str, u64>,
}

fn is_skill(name: &str) -> bool {
    matches!(name, "skills_list" | "skill_view" | "skill_manage")
}

fn is_web(name: &str) -> bool {
    name.starts_with("web_") || name.starts_with("browser_")
}

impl Metrics {
    fn of<'a>(sessions: impl Iterator<Item = &'a SessionStats>, zone: &TimeZone) -> Self {
        let mut m = Self::default();
        let mut tools = BTreeSet::new();
        let mut models = BTreeSet::new();
        let mut providers = BTreeSet::new();
        let mut days = BTreeSet::new();
        for s in sessions {
            m.add("sessions", 1);
            m.max("chain", s.longest_chain);
            m.max("longest", s.messages);
            m.add("calls", s.tools.values().sum());
            m.add("errors", s.errors);
            m.add("recoveries", s.recoveries);
            m.add("skills", s.calls(is_skill));
            m.add("memory", s.calls(|n| n == "memory"));
            m.add("delegations", s.calls(|n| n == "delegate_task"));
            m.add("web", s.calls(is_web));
            tools.extend(s.tools.keys().cloned());
            models.extend(s.models.iter().cloned());
            providers.extend(s.providers.iter().cloned());
            if let Ok(at) = Timestamp::from_millisecond(s.started_at_ms) {
                let local = at.to_zoned(zone.clone());
                let hour = local.hour();
                m.add("night", u64::from((0..5).contains(&hour)));
                m.add("early", u64::from((5..8).contains(&hour)));
                let weekend = matches!(local.weekday(), Weekday::Saturday | Weekday::Sunday);
                m.add("weekend", u64::from(weekend));
                days.insert(local.date());
            }
        }
        m.values.insert("tools", count(&tools));
        m.values.insert("models", count(&models));
        m.values.insert("providers", count(&providers));
        m.values.insert("streak", streak(&days));
        m
    }

    fn add(&mut self, key: &'static str, n: u64) {
        *self.values.entry(key).or_default() += n;
    }

    fn max(&mut self, key: &'static str, n: u64) {
        let v = self.values.entry(key).or_default();
        *v = (*v).max(n);
    }

    fn value(&self, metric: Metric) -> u64 {
        let key = match metric {
            Metric::LongestToolChain => "chain",
            Metric::DistinctTools => "tools",
            Metric::ToolCalls => "calls",
            Metric::Recoveries => "recoveries",
            Metric::ToolErrors => "errors",
            Metric::DayStreak => "streak",
            Metric::SkillUses => "skills",
            Metric::MemoryUses => "memory",
            Metric::DistinctModels => "models",
            Metric::DistinctProviders => "providers",
            Metric::NightSessions => "night",
            Metric::EarlySessions => "early",
            Metric::WeekendSessions => "weekend",
            Metric::LongestSession => "longest",
            Metric::Sessions => "sessions",
            Metric::Delegations => "delegations",
            Metric::WebUses => "web",
        };
        self.values.get(key).copied().unwrap_or(0)
    }
}

fn count<T>(set: &BTreeSet<T>) -> u64 {
    u64::try_from(set.len()).unwrap_or(u64::MAX)
}

/// The longest run of consecutive dates.
fn streak(days: &BTreeSet<Date>) -> u64 {
    let mut best = 0_u64;
    let mut run = 0_u64;
    let mut previous: Option<Date> = None;
    for &day in days {
        run = match previous.and_then(|p| p.tomorrow().ok()) {
            Some(next) if next == day => run + 1,
            _ => 1,
        };
        best = best.max(run);
        previous = Some(day);
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streaks_count_consecutive_days() {
        let d = |m, day| jiff::civil::date(2026, m, day);
        let days: BTreeSet<Date> = [d(1, 30), d(1, 31), d(2, 1), d(2, 3), d(2, 4)]
            .into_iter()
            .collect();
        assert_eq!(streak(&days), 3);
        assert_eq!(streak(&BTreeSet::new()), 0);
    }

    #[test]
    fn badges_show_the_next_tier() {
        let owl = super::super::catalog::find("night-owl").unwrap();
        let shown = badge(owl, 4, None);
        assert_eq!(shown.next_tier.as_deref(), Some("silver"));
        assert_eq!(shown.next_threshold, Some(10));
        assert_eq!(badge(owl, 5_000, None).next_tier, None);
    }
}

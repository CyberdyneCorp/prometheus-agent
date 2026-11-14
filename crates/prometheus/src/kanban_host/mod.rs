//! The binary's side of kanban (spec: kanban): the dispatcher's settings
//! from `[kanban]`, which profiles exist, how workers are spawned
//! ([`spawn`]), the worker itself ([`worker`]), the `kanban_*` tool port
//! ([`tool`]) and notification texts ([`notice`]).

pub mod notice;
pub mod spawn;
pub mod tool;
pub mod worker;

use std::path::{Path, PathBuf};

use pmt_config::ProfileName;
use pmt_config::schema::KanbanConfig;
use pmt_kanban::dispatch::{Dispatcher, Settings};
use pmt_kanban::lease::Holder;
use pmt_kanban::{Boards, Breaker, Profiles};

/// Whether `name` is a profile of the home at `home`.
#[must_use]
pub fn profile_exists(home: &Path, name: &str) -> bool {
    ProfileName::parse(name).is_ok_and(|n| {
        n.as_str() == pmt_config::layout::DEFAULT_PROFILE
            || home.join("profiles").join(n.as_str()).is_dir()
    })
}

/// The profiles of one home.
#[derive(Debug, Clone)]
pub struct HomeProfiles(pub PathBuf);

impl Profiles for HomeProfiles {
    fn exists(&self, name: &str) -> bool {
        profile_exists(&self.0, name)
    }
}

/// The dispatcher settings of `config`.
#[must_use]
pub fn settings(config: &KanbanConfig) -> Settings {
    Settings {
        max_running: config
            .max_running
            .unwrap_or_else(pmt_kanban::dispatch::default_max_running),
        max_running_per_profile: config.max_running_per_profile,
        dispatch_profiles: config.dispatch_profiles.clone(),
        default_assignee: config.default_assignee.clone(),
        breaker: breaker(config),
        claim_ttl_ms: ttl_ms(config),
        review_dispatch: config.review_dispatch,
    }
}

/// The failure breaker of `config`.
#[must_use]
pub fn breaker(config: &KanbanConfig) -> Breaker {
    Breaker {
        failure_limit: config.failure_limit,
        cooldown_ms: secs_ms(config.rate_limit_cooldown_secs),
    }
}

/// `kanban.claim_ttl_secs` in ms.
#[must_use]
pub fn ttl_ms(config: &KanbanConfig) -> i64 {
    secs_ms(config.claim_ttl_secs)
}

/// Seconds as ms.
#[must_use]
pub fn secs_ms(secs: u64) -> i64 {
    i64::try_from(secs.saturating_mul(1000)).unwrap_or(i64::MAX)
}

/// Runs one dispatcher tick for the home at `home` as `kind` (`daemon`
/// or `cli`) with `config`'s settings, spawning workers with `launch`'s
/// locale and `PATH`.
///
/// # Errors
///
/// `kanban.lease_held`, store errors.
pub fn tick(
    home: &Path,
    kind: &str,
    config: &KanbanConfig,
    launch: &pmt_core::env::LaunchEnv,
) -> Result<Vec<pmt_kanban::dispatch::Summary>, pmt_core::Error> {
    let boards = Boards::open(home)?;
    let spawner = spawn::ProcessSpawner::new(home, launch)?;
    let profiles = HomeProfiles(home.to_path_buf());
    let dispatcher = Dispatcher {
        boards: &boards,
        holder: Holder::current(kind),
        settings: settings(config),
        profiles: &profiles,
        spawner: &spawner,
    };
    dispatcher.tick(pmt_kanban::now_ms())
}

/// Renews this process's lease on every live board of the home (between
/// ticks longer apart than the lease's renewal period).
///
/// # Errors
///
/// `kanban.lease_held` when another dispatcher took a board over.
pub fn renew(home: &Path, kind: &str) -> Result<(), pmt_core::Error> {
    let boards = Boards::open(home)?;
    let holder = Holder::current(kind);
    for slug in boards.live()? {
        boards
            .board(&slug)?
            .acquire_lease(&holder, pmt_kanban::now_ms())?;
    }
    Ok(())
}

/// Gives back this process's leases (a stopping daemon), so another
/// dispatcher need not wait for them to expire.
pub fn release(home: &Path, kind: &str) {
    let Ok(boards) = Boards::open(home) else {
        return;
    };
    let holder = Holder::current(kind);
    for slug in boards.live().unwrap_or_default() {
        if let Ok(mut board) = boards.board(&slug) {
            let _ = board.release_lease(&holder);
        }
    }
}

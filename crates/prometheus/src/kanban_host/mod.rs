//! The binary's side of kanban (spec: kanban): the dispatcher's settings
//! from `[kanban]`, which profiles exist, how workers are spawned
//! ([`spawn`]), the worker itself ([`worker`]), the `kanban_*` tool port
//! ([`tool`]) and notification texts ([`notice`]).

pub mod assignees;
pub mod fetch;
pub mod github;
pub mod goal;
pub mod hooks;
pub mod lanes;
pub mod notice;
pub mod orchestrate;
pub mod spawn;
pub mod tool;
pub mod worker;

use std::path::Path;
use std::sync::Arc;

use pmt_config::schema::KanbanConfig;
use pmt_config::{ProfileName, ProfileScope};
use pmt_kanban::dispatch::{Dispatcher, Settings};
use pmt_kanban::lease::Holder;
use pmt_kanban::{Boards, Breaker};

/// Whether `name` is a profile of the home at `home`.
#[must_use]
pub fn profile_exists(home: &Path, name: &str) -> bool {
    ProfileName::parse(name).is_ok_and(|n| {
        n.as_str() == pmt_config::layout::DEFAULT_PROFILE
            || home.join("profiles").join(n.as_str()).is_dir()
    })
}

/// The board bound to the project owning `cwd`, else to the active
/// project, in `scope`'s projects (spec: projects, Kanban board binding):
/// the board kanban commands and tools select by default.
#[must_use]
pub fn project_board(scope: &ProfileScope, cwd: &Path) -> Option<String> {
    let store = pmt_store::Store::open(scope).ok()?;
    let project = match store.project_owning(cwd).ok()? {
        Some(project) => project,
        None => store.active_project().ok()??,
    };
    project.board
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
        windows: pmt_kanban::dispatch::guard::Windows {
            success_ms: secs_ms(config.respawn_success_window_secs),
            pr_ms: secs_ms(config.respawn_pr_window_secs),
        },
    }
}

/// The boards of `scope`'s home as this process works them: completion
/// contracts read GitHub with the assignee's credential, and lifecycle
/// events queue on `hooks`.
///
/// # Errors
///
/// `home.io` when the kanban directory cannot be created.
pub fn boards(
    scope: &ProfileScope,
    hooks: &Arc<hooks::HookQueue>,
) -> Result<Boards, pmt_core::Error> {
    Ok(Boards::open(scope.home())?
        .with_pr_checks(Arc::new(github::GitHub::new(scope.home())))
        .with_observer(Arc::clone(hooks) as Arc<dyn pmt_kanban::observe::Observer>))
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

/// Runs one dispatcher tick for `scope`'s home as `kind` (`daemon` or
/// `cli`) with its `[kanban]` settings, spawning workers with `launch`'s
/// locale and `PATH` and lanes from its enabled plugins. With
/// `kanban.auto_decompose` it first specifies triage tasks and decomposes
/// orchestrator tasks. Lifecycle hooks fire after the tick, outside it.
///
/// # Errors
///
/// `kanban.lease_held` (an `on_kanban_dispatch_tick` with outcome
/// `skipped_locked` still fires), store errors.
pub fn tick(
    scope: &ProfileScope,
    kind: &str,
    launch: &pmt_core::env::LaunchEnv,
) -> Result<Vec<pmt_kanban::dispatch::Summary>, pmt_core::Error> {
    let config = &scope.config().kanban;
    let queue = hooks::HookQueue::new(scope);
    let boards = boards(scope, &queue)?;
    if config.auto_decompose {
        orchestrate::auto(scope, &boards, launch);
    }
    let spawner = spawn::ProcessSpawner::new(scope, launch)?;
    let profiles = assignees::Assignees::of(scope);
    let dispatcher = Dispatcher {
        boards: &boards,
        holder: Holder::current(kind),
        settings: settings(config),
        profiles: &profiles,
        spawner: &spawner,
    };
    let result = dispatcher.tick(pmt_kanban::now_ms());
    match &result {
        Ok(summaries) => queue.tick(summaries, &[], false),
        Err(err) if err.code().code == pmt_core::error::codes::KANBAN_LEASE_HELD.code => {
            queue.tick(&[], &boards.live().unwrap_or_default(), false);
        }
        Err(_) => {}
    }
    queue.flush(scope);
    result
}

/// The dispatcher's dry run for `scope`'s home: nothing changes.
///
/// # Errors
///
/// Store errors.
pub fn plan(
    scope: &ProfileScope,
    launch: &pmt_core::env::LaunchEnv,
) -> Result<Vec<pmt_kanban::dispatch::plan::Plan>, pmt_core::Error> {
    let boards = Boards::open(scope.home())?;
    let spawner = spawn::ProcessSpawner::new(scope, launch)?;
    let profiles = assignees::Assignees::of(scope);
    let dispatcher = Dispatcher {
        boards: &boards,
        holder: Holder::current("cli"),
        settings: settings(&scope.config().kanban),
        profiles: &profiles,
        spawner: &spawner,
    };
    dispatcher.plan(pmt_kanban::now_ms())
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

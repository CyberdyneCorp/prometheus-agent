//! Session automation in the hosts (spec: session-automation): the glue
//! between the host-agnostic engine (`pmt-automation`) and a session —
//! its stored rows, its judge route, its terminal (quality gates) and its
//! background processes (wait barriers).
//!
//! Two hosts drive automated turns: the line interface for a session it
//! runs in-process ([`crate::chat::repl`]) and the daemon for every
//! session it serves (clients, the messaging gateway). Both call
//! [`boundary::after_turn`] when a turn ends and [`boundary::due`] when
//! the session is idle, so every surface behaves the same; the commands
//! ([`commands`]) only change stored state.

pub mod boundary;
pub mod commands;
mod gates;
pub mod stored;

use std::sync::{Arc, Mutex};

use pmt_automation::judge::Process;
use pmt_automation::{Automation, Limits};
use pmt_config::schema::Config;
use pmt_core::id::SessionId;
use pmt_tools::environments::Environments;

pub use stored::{Stored, load};

/// The configured bounds (`[goals]`, `[loops]`, `[heartbeat]`).
#[must_use]
pub fn limits(config: &Config) -> Limits {
    Limits {
        goal_max_turns: config.goals.max_turns,
        gate_max_retries: config.goals.gate_max_retries,
        max_wait_secs: config.goals.max_wait_secs,
        loop_min_interval_secs: config.loops.min_interval_secs,
        loop_max_ticks: config.loops.max_ticks,
        self_paced_floor_secs: config.loops.self_paced_floor_secs,
        self_paced_ceiling_secs: config.loops.self_paced_ceiling_secs,
        heartbeat_min_interval_secs: config.heartbeat.min_interval_secs,
    }
}

/// The clock automation reads (ms since the epoch).
#[must_use]
pub fn now_ms() -> i64 {
    jiff::Timestamp::now().as_millisecond()
}

/// Process liveness for one session: local pids, and the background
/// processes the session itself started.
pub struct LocalProbe {
    session: String,
}

impl LocalProbe {
    /// The probe of `session`.
    #[must_use]
    pub fn of(session: SessionId) -> Self {
        Self {
            session: session.to_string(),
        }
    }

    fn running(&self) -> Vec<pmt_tools::terminal::process::Running> {
        Environments::process()
            .session(&self.session)
            .and_then(|env| env.terminal.clone())
            .map(|terminal| terminal.processes().own_running())
            .unwrap_or_default()
    }

    /// The session's own running background processes, as the judge sees
    /// them.
    #[must_use]
    pub fn processes(&self) -> Vec<Process> {
        self.running()
            .into_iter()
            .map(|p| Process {
                id: p.id,
                pid: p.pid,
                command: p.command,
            })
            .collect()
    }
}

impl pmt_automation::goal::Probe for LocalProbe {
    fn pid_start(&self, pid: u32) -> Option<u64> {
        if pid == 0 {
            return None;
        }
        pmt_core::runtime::process_start_time(pid)
    }

    fn process(&self, id: &str) -> Option<u32> {
        self.running()
            .into_iter()
            .find(|p| p.id == id)
            .map(|p| p.matches)
    }
}

/// Which host drives a session's automation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Host {
    /// The line interface running the session in-process.
    Line,
    /// The daemon (clients and the messaging gateway).
    Daemon,
}

impl Host {
    /// Whether this host fires an automation its `owner` surface set: one
    /// set from a messaging chat only fires from the daemon's gateway.
    #[must_use]
    pub fn drives(self, owner: &str) -> bool {
        self == Self::Daemon || owner != OWNER_GATEWAY
    }
}

/// The owner surface of automation set from a messaging chat.
pub const OWNER_GATEWAY: &str = "gateway";

/// One lock per session around read-modify-write of its rows (a command
/// and a boundary never interleave their writes).
fn session_lock(id: SessionId) -> Arc<Mutex<()>> {
    static LOCKS: std::sync::LazyLock<Mutex<std::collections::HashMap<SessionId, Arc<Mutex<()>>>>> =
        std::sync::LazyLock::new(Mutex::default);
    let mut locks = LOCKS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Arc::clone(locks.entry(id).or_default())
}

/// Runs `change` on the stored automation of `id` under its lock and
/// writes back what changed.
///
/// # Errors
///
/// Store errors.
pub fn modify<T>(
    store: &pmt_store::Store,
    id: SessionId,
    change: impl FnOnce(&mut Stored) -> T,
) -> Result<T, pmt_core::error::Error> {
    let lock = session_lock(id);
    let _held = lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut stored = load(store, id)?;
    let before = stored.clone();
    let out = change(&mut stored);
    stored::save(store, id, &before, &stored)?;
    Ok(out)
}

/// The live rows of `id` (`/agents`, the line interface's status line).
#[must_use]
pub fn rows(
    text: &dyn pmt_core::text::Catalog,
    store: &pmt_store::Store,
    id: SessionId,
    limits: &Limits,
) -> Vec<String> {
    load(store, id)
        .map(|s| s.automation.rows(text, now_ms(), limits))
        .unwrap_or_default()
}

/// Whether `automation` keeps its session from the idle reaper.
#[must_use]
pub fn keeps_alive(store: &pmt_store::Store, id: SessionId) -> bool {
    load(store, id).is_ok_and(|s| s.automation.keeps_alive())
}

/// Drops every automation of `id` (`/new`, `/reset`).
pub fn clear(store: &pmt_store::Store, id: SessionId) {
    if let Err(err) = store.clear_automation(id) {
        tracing::warn!(error = %err, "automation not cleared");
    }
}

/// The automation of `id` (empty when it cannot be read).
#[must_use]
pub fn of(store: &pmt_store::Store, id: SessionId) -> Automation {
    load(store, id).map(|s| s.automation).unwrap_or_default()
}

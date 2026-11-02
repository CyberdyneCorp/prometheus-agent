//! Crash recovery (spec: messaging-gateway, Crash recovery and clean
//! shutdown marker; Stuck-loop suspension). A stop whose conversations
//! finished within `gateway.shutdown.drain_timeout_secs` leaves
//! `<home>/run/clean_shutdown`; a start that finds it deletes it and skips
//! recovery. Otherwise every turn that was running is marked
//! `resume_pending` (its chat is told once how to run it again), a session
//! interrupted by too many consecutive restarts is suspended, and a send
//! that was in flight becomes `unknown` (never resent automatically).

use std::io;
use std::path::{Path, PathBuf};

use pmt_core::error::{Error, codes};
use pmt_store::gateway::outbox::{NewOutbound, OutboundKind};

use super::Shared;

/// The clean-shutdown marker's file name under `<home>/run/`.
pub const CLEAN_SHUTDOWN: &str = "clean_shutdown";

/// What an interrupted conversation is told after a restart.
pub const RESTARTED: &str = "The agent restarted while working on your last message. \
    Reply /retry to run it again.";

fn marker(home: &Path) -> PathBuf {
    home.join("run").join(CLEAN_SHUTDOWN)
}

/// Removes the marker; `true` when the last stop was clean.
///
/// # Errors
///
/// `gateway.recovery` when the marker exists but cannot be removed (the
/// daemon must not start: the next start would skip recovery again).
pub(crate) fn take_marker(home: &Path) -> Result<bool, Error> {
    let path = marker(home);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(Error::io_doing(
            &codes::GATEWAY_RECOVERY,
            "cannot remove the clean-shutdown marker",
            &path,
            &e,
        )),
    }
}

/// Writes the marker after a clean stop.
pub(crate) fn write_marker(home: &Path) {
    let path = marker(home);
    if let Err(err) = std::fs::write(&path, b"") {
        tracing::warn!(path = %path.display(), "clean-shutdown marker not written: {err}");
    }
}

/// Recovers `profile` after an unclean stop.
pub(crate) fn recover(shared: &Shared, profile: &str) {
    let (Some(store), Some(scope)) = (shared.store(profile), shared.agents.scope(profile)) else {
        return;
    };
    match store.abandon_in_flight() {
        Ok(0) => {}
        Ok(n) => tracing::warn!(
            profile,
            "{n} message(s) were being sent at the crash: unknown"
        ),
        Err(err) => tracing::error!(profile, "outbox recovery failed: {err}"),
    }
    let threshold = scope.config().gateway.sessions.stuck_loop_threshold;
    let interrupted = match store.recover_interrupted(threshold) {
        Ok(found) => found,
        Err(err) => {
            tracing::error!(profile, "turn recovery failed: {err}");
            return;
        }
    };
    for turn in interrupted {
        if turn.suspended {
            tracing::warn!(
                profile,
                session_id = %turn.session_id,
                "session suspended after {threshold} restarts during its turns"
            );
        }
        let Some(route) = turn.route else {
            continue;
        };
        let restarts = store
            .routed_session(turn.session_id)
            .ok()
            .flatten()
            .map_or(0, |s| s.restart_count);
        let notice = NewOutbound {
            idempotency_key: format!("{}:restart:{restarts}", turn.session_id),
            session_id: Some(turn.session_id.to_string()),
            platform: route.platform,
            chat_id: route.chat_id,
            thread_id: route.thread_id,
            reply_to: None,
            edit_of: None,
            kind: OutboundKind::System,
            body: RESTARTED.to_owned(),
        };
        shared.deliver(profile, &notice);
    }
}

//! Crash recovery (spec: messaging-gateway, Crash recovery and clean
//! shutdown marker; Stuck-loop suspension). A stop whose conversations
//! finished within `gateway.shutdown.drain_timeout_secs` leaves
//! `<home>/run/clean_shutdown`; a start that finds it deletes it and skips
//! recovery. Otherwise every turn that was running is marked
//! `resume_pending` (its chat is told once how to run it again), a session
//! interrupted by too many consecutive restarts is suspended, and a send
//! that was in flight becomes `unknown` (never resent automatically).
//!
//! A profile with `agent.resume_on_crash = "auto"` does not ask: a turn
//! whose journal is unfinished is resumed at startup as a turn of its
//! chat's lane, which delivers the answer like any reply (spec:
//! agent-turn-loop, Crash resume mid-tool-batch). The restart still
//! counts toward stuck-loop suspension, and a suspended session is never
//! resumed.

use std::io;
use std::path::{Path, PathBuf};

use pmt_config::schema::ResumeOnCrash;
use pmt_core::error::{Error, codes};
use pmt_core::id::SessionId;
use pmt_store::gateway::outbox::{NewOutbound, OutboundKind};

use super::Shared;

/// The clean-shutdown marker's file name under `<home>/run/`.
pub const CLEAN_SHUTDOWN: &str = "clean_shutdown";

/// The `origin` of the lane turn that resumes an interrupted one: the
/// host resumes the session's unfinished turn instead of storing input.
pub const RESUME_ORIGIN: &str = "resume";

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

/// Recovers `profile` after an unclean stop; the sessions whose turns
/// resume by themselves (`agent.resume_on_crash = "auto"`).
pub(crate) fn recover(shared: &Shared, profile: &str) -> Vec<SessionId> {
    let (Some(store), Some(scope)) = (shared.store(profile), shared.agents.scope(profile)) else {
        return Vec::new();
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
            return Vec::new();
        }
    };
    let auto = scope.config().agent.resume_on_crash == ResumeOnCrash::Auto;
    let mut resumes = Vec::new();
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
        if auto && !turn.suspended && resumes_by_itself(&store, turn.session_id) {
            resumes.push(turn.session_id);
            continue;
        }
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
            body: pmt_i18n::t!(shared.strings(profile), "gateway.restarted_retry"),
            attachments: Vec::new(),
            notify: None,
        };
        shared.deliver(profile, &notice);
    }
    resumes
}

/// Whether `session`'s turn resumes without asking: its journal holds an
/// unfinished turn (one that crashed before its first journal record has
/// nothing to resume, so its chat is asked as usual) and its hold is
/// lifted.
fn resumes_by_itself(store: &pmt_store::Store, session: SessionId) -> bool {
    match store.unfinished_turn(session) {
        Ok(Some(_)) => {}
        Ok(None) => return false,
        Err(err) => {
            tracing::warn!(%session, "turn journal unreadable: {err}");
            return false;
        }
    }
    store.discard_resume(session).unwrap_or_else(|err| {
        tracing::warn!(%session, "the interrupted turn stays held: {err}");
        false
    })
}

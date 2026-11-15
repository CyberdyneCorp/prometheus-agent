//! The automatic trigger at session end (spec: harness-evolution,
//! Automatic triggers after failed or long tasks).

use pmt_config::ProfileScope;
use pmt_core::id::SessionId;
use pmt_store::{EndReason, Store};

/// Queues a job when the session that just ended with `reason` qualifies;
/// the notice telling the user, if one was queued. Failures are logged:
/// they never fail the session's end.
#[must_use]
pub fn after_end(
    scope: &ProfileScope,
    store: &Store,
    id: SessionId,
    reason: EndReason,
) -> Option<String> {
    if crate::surface::require("evolve").is_err() {
        return None;
    }
    let profile = super::profile(scope, Some(store));
    match pmt_evolve::job::auto::after_session(&profile, store, id, reason) {
        Ok(Some(job)) => Some(format!(
            "harness evolution job {} queued after this session (prometheus evolve list)",
            job.id
        )),
        Ok(None) => None,
        Err(err) => {
            tracing::warn!(session = %id, error = %err, "automatic evolution job not queued");
            None
        }
    }
}

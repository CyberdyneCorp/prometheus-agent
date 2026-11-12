//! A session's trace (spec: observability, Span model; Trace context
//! propagation): every session gets a [`SessionTrace`] whatever the export
//! setting, so its children, plugin and bridge calls and log records share
//! its trace id, and every event it records passes through it.

use std::sync::Arc;

use pmt_agent::turn::{EventSink, NoEvents, Traced};
use pmt_obs::otel::{Identity, SessionTrace, TraceContext};
use pmt_obs::trajectory::writer::TrajectoryWriter;

use super::progress;

/// The trace of `session` of the profile `scope`, under `parent` when a
/// caller (a cron run) started it, else under its parent session's open
/// delegation.
pub(super) fn open(
    scope: &pmt_config::ProfileScope,
    session: &pmt_store::Session,
    parent: Option<TraceContext>,
) -> Arc<SessionTrace> {
    let redactor_scope = scope.clone();
    SessionTrace::open(
        Identity {
            profile: scope.name().as_str().to_owned(),
            session_id: session.id.to_string(),
            root_session_id: session.root_session_id.to_string(),
            source: session.source.as_str().to_owned(),
        },
        session.parent_session_id.map(|p| p.to_string()).as_deref(),
        parent,
        // Captured content is redacted as the trajectory is, loaded only
        // when a span first captures some.
        Some(Box::new(move || {
            pmt_obs::redaction::profile_redactor(&redactor_scope)
                .ok()
                .flatten()
        })),
    )
}

/// What a session records: its trajectory (when on) and its trace.
pub(super) struct Recording {
    pub trajectory: Option<Arc<TrajectoryWriter>>,
    pub trace: Arc<SessionTrace>,
    /// `--output-format stream-json`: stdout (a top-level session of this
    /// process only).
    pub stream: Option<Arc<dyn EventSink>>,
}

/// Where a session's events go: its trace, then its trajectory (when on)
/// and its daemon clients (when attached).
pub(super) fn events_to(
    recording: &Recording,
    clients: Option<Arc<dyn EventSink>>,
) -> Arc<dyn EventSink> {
    let events = recording
        .trajectory
        .clone()
        .map_or_else(|| Arc::new(NoEvents) as Arc<dyn EventSink>, |t| t as _);
    let events = match clients.or_else(|| recording.stream.clone()) {
        Some(clients) => Arc::new(progress::Both(events, clients)),
        None => events,
    };
    Arc::new(Traced::new(events, Arc::clone(&recording.trace)))
}

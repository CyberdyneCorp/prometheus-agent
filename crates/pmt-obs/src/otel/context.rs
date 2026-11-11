//! Trace context (spec: observability, Trace context propagation): the
//! W3C `traceparent` form, the trace id of a session, and the in-process
//! directory of live sessions that children and outbound calls read their
//! parent context from.
//!
//! A session's trace id is derived from its root session id, so every
//! session of one lineage (compression continuations, forks, subagents and
//! background delegations) shares one trace and the id is the same in the
//! session's log records, its trajectory's `session.start` and its
//! exported spans without passing it around.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, LazyLock, Mutex, PoisonError, Weak};

use sha2::{Digest as _, Sha256};

use super::spans::SessionTrace;

/// A span's position in a trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TraceContext {
    /// 16-byte trace id.
    pub trace_id: [u8; 16],
    /// 8-byte span id.
    pub span_id: [u8; 8],
}

impl TraceContext {
    /// The `traceparent` header value (`00-<trace>-<span>-01`).
    #[must_use]
    pub fn traceparent(&self) -> String {
        format!("00-{}-{}-01", hex(&self.trace_id), hex(&self.span_id))
    }

    /// Parses a version-00 `traceparent`; `None` when malformed or all
    /// zeros.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        let mut parts = value.trim().split('-');
        let (version, trace, span, flags) =
            (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
        if version != "00" || flags.len() != 2 || parts.next().is_some() {
            return None;
        }
        let trace_id: [u8; 16] = unhex(trace)?.try_into().ok()?;
        let span_id: [u8; 8] = unhex(span)?.try_into().ok()?;
        (trace_id != [0; 16] && span_id != [0; 8]).then_some(Self { trace_id, span_id })
    }

    /// The trace id as 32 lowercase hex digits.
    #[must_use]
    pub fn trace_hex(&self) -> String {
        hex(&self.trace_id)
    }
}

impl fmt::Display for TraceContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.traceparent())
    }
}

/// The trace id of every session whose lineage root is `root_session_id`.
#[must_use]
pub fn trace_id_for_root(root_session_id: &str) -> [u8; 16] {
    let digest = Sha256::digest(root_session_id.as_bytes());
    let mut id = [0u8; 16];
    id.copy_from_slice(&digest[..16]);
    if id == [0; 16] {
        id[15] = 1;
    }
    id
}

/// [`trace_id_for_root`] as 32 hex digits (log records, `session.start`).
#[must_use]
pub fn trace_hex_for_root(root_session_id: &str) -> String {
    hex(&trace_id_for_root(root_session_id))
}

/// The context of the `cron.run` span of run `run_id`: derived from the
/// run id, so the scheduler (which records the span) and the run's agent
/// session (which hangs under it) agree without passing it along.
#[must_use]
pub fn cron_run(run_id: &str) -> TraceContext {
    derived(&format!("cron.run:{run_id}"))
}

/// A context derived from `seed` (a fresh trace with one span).
#[must_use]
pub fn derived(seed: &str) -> TraceContext {
    let digest = Sha256::digest(seed.as_bytes());
    let mut span_id = [0u8; 8];
    span_id.copy_from_slice(&digest[16..24]);
    if span_id == [0; 8] {
        span_id[7] = 1;
    }
    TraceContext {
        trace_id: trace_id_for_root(seed),
        span_id,
    }
}

/// A new random span id.
#[must_use]
pub fn new_span_id() -> [u8; 8] {
    loop {
        let random = ulid::Ulid::generate().random() & u128::from(u64::MAX);
        let id = u64::try_from(random).unwrap_or(1).to_be_bytes();
        if id != [0; 8] {
            return id;
        }
    }
}

/// Lowercase hex of `bytes`.
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    use fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
        .collect()
}

static LIVE: LazyLock<Mutex<HashMap<String, Weak<SessionTrace>>>> = LazyLock::new(Mutex::default);

/// Makes `trace` findable by its session id while it lives.
pub(crate) fn register(session_id: &str, trace: &Arc<SessionTrace>) {
    let mut live = LIVE.lock().unwrap_or_else(PoisonError::into_inner);
    live.retain(|_, weak| weak.strong_count() > 0);
    live.insert(session_id.to_owned(), Arc::downgrade(trace));
}

/// The live trace of `session_id`.
#[must_use]
pub fn session(session_id: &str) -> Option<Arc<SessionTrace>> {
    LIVE.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(session_id)
        .and_then(Weak::upgrade)
}

/// The trace id of `session_id`: its live trace's, else the session taken
/// as its own root.
#[must_use]
pub fn trace_hex_of(session_id: &str) -> String {
    session(session_id).map_or_else(|| trace_hex_for_root(session_id), |t| t.trace_hex())
}

/// The context a call made by `session_id` right now runs under (its
/// innermost open span: a tool call, else the turn, else the session).
#[must_use]
pub fn current(session_id: &str) -> Option<TraceContext> {
    session(session_id).map(|t| t.current())
}

/// The `traceparent` to send to an external host (model provider, MCP
/// server) for a call of `session_id`: only with
/// `observability.otel.propagate_external = true`.
#[must_use]
pub fn external_traceparent(session_id: &str) -> Option<String> {
    super::exporter()
        .filter(super::Exporter::propagates_external)
        .and_then(|_| current(session_id))
        .map(|c| c.traceparent())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traceparent_round_trips_and_rejects_malformed_values() {
        let context = TraceContext {
            trace_id: trace_id_for_root("ses_root"),
            span_id: [1, 2, 3, 4, 5, 6, 7, 8],
        };
        let text = context.traceparent();
        assert_eq!(text.len(), 55);
        assert_eq!(TraceContext::parse(&text), Some(context));
        assert_eq!(TraceContext::parse("01-00-00-01"), None);
        assert_eq!(
            TraceContext::parse("00-00000000000000000000000000000000-0102030405060708-01"),
            None
        );
    }

    #[test]
    fn every_session_of_a_lineage_shares_the_trace_id() {
        assert_eq!(trace_hex_for_root("ses_a"), trace_hex_for_root("ses_a"));
        assert_ne!(trace_hex_for_root("ses_a"), trace_hex_for_root("ses_b"));
        assert_eq!(trace_hex_for_root("ses_a").len(), 32);
    }
}

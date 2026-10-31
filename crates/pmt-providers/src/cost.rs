//! Per-call cost accounting (spec: model-providers, Per-call cost
//! accounting; observability, Per-call usage ledger): every call that
//! reports usage appends exactly one ledger row, priced by the catalog,
//! with the route's `billing_source`.

use std::sync::Arc;

use pmt_core::error::Error;
use pmt_core::id::SessionId;
use pmt_obs::usage::{Call, UsageRecorder};
use pmt_store::UsageEvent;

use crate::event::Usage;
use crate::request::Request;
use crate::resolve::Route;

/// A cap on implicit stream idle timeouts (spec: agent-turn-loop,
/// Wall-clock run budget): applied to every route whose provider is not in
/// `explicit`, and only ever lowers a timeout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleCap {
    /// The longest idle gap allowed.
    pub cap: std::time::Duration,
    /// Providers whose `stream_idle_timeout_secs` the user configured.
    pub explicit: Vec<String>,
}

impl StaleCap {
    /// `route` with its idle timeout capped, unless it was configured.
    #[must_use]
    pub fn apply(&self, route: &Route) -> Route {
        let mut route = route.clone();
        if !self.explicit.contains(&route.provider) {
            route.timeouts.idle = route.timeouts.idle.min(self.cap);
        }
        route
    }
}

/// Who makes a call, for its ledger row (and its per-call limits).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallContext {
    /// The calling session.
    pub session_id: SessionId,
    /// The turn, when inside one.
    pub turn_id: Option<String>,
    /// Dotted purpose (`main`, `auxiliary.title`...).
    pub purpose: String,
    /// The call this one serves.
    pub parent_call_id: Option<String>,
    /// Caps implicit stream idle timeouts (a turn under a run budget).
    pub stale_cap: Option<StaleCap>,
}

impl CallContext {
    /// A context with `purpose` and no turn.
    #[must_use]
    pub fn new(session_id: SessionId, purpose: &str) -> Self {
        Self {
            session_id,
            turn_id: None,
            purpose: purpose.to_owned(),
            parent_call_id: None,
            stale_cap: None,
        }
    }
}

/// A pending ledger row: written once the call's usage is known.
#[derive(Debug)]
pub(crate) struct Pending {
    recorder: Arc<UsageRecorder>,
    call: Call,
}

impl Pending {
    pub(crate) fn new(
        recorder: Arc<UsageRecorder>,
        route: &Route,
        request: &Request,
        ctx: &CallContext,
    ) -> Self {
        let mut call = Call::new(ctx.session_id, &ctx.purpose, &route.provider, &route.model);
        call.turn_id.clone_from(&ctx.turn_id);
        call.parent_call_id.clone_from(&ctx.parent_call_id);
        call.billing_source = route.billing_source();
        call.request_hash = Some(request.hash(&route.provider, &route.model));
        Self { recorder, call }
    }

    /// Uses `call_id` for the row (events emitted before the call ends
    /// name it).
    pub(crate) fn with_call_id(mut self, call_id: &str) -> Self {
        self.call.call_id = Some(call_id.to_owned());
        self
    }

    /// Prices and appends the row for `usage`.
    pub(crate) fn record(self, usage: &Usage) -> Result<UsageEvent, Error> {
        let mut call = self.call;
        call.tokens = usage.tokens();
        call.reported_usd = usage.cost_usd;
        self.recorder.record(call)
    }
}

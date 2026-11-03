//! What plugins see of approvals (spec: plugin-system, Approval and
//! human-input observer hooks; Approval transports).
//!
//! The gate tells [`ApprovalHooks`] before and after every decision it
//! requests (observers: they cannot change it) and, when the profile
//! selects a plugin approval transport, asks the transport instead of
//! the surface. The transport gets an immutable request with the choices
//! it may answer; the gate accepts only an answer bound to the request's
//! id and digest with one of those choices, so a transport can neither
//! widen a once-only request nor answer another one. Hardline blocks are
//! decided before the gate ever asks.

use std::time::Duration;

use super::gate::{ApprovalRequest, Decision};
use crate::dispatch::SeamFuture;

/// What a transport is shown (immutable; the redaction is the embedder's).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportRequest {
    /// A fresh id per request.
    pub id: String,
    /// `sha256:` of the request's content, binding the answer to it.
    pub digest: String,
    /// The request.
    pub request: ApprovalRequest,
    /// The choices this request offers (`once` and `deny` only when
    /// nothing may be remembered).
    pub choices: Vec<Decision>,
    /// `approvals.timeout_secs`.
    pub timeout: Duration,
}

/// What a transport's answer amounts to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportOutcome {
    /// A valid, bound answer.
    Decided(Decision),
    /// Ask the surface as if no transport were selected
    /// (`plugins.approval_transport_fallback = "builtin"`).
    Builtin,
    /// Denied: an error, a timeout, a stale, unbound or widening answer,
    /// or a missing transport.
    Denied(String),
}

/// The plugin side of a session's approvals.
pub trait ApprovalHooks: Send + Sync + std::fmt::Debug {
    /// `pre_approval_request` before a decision is requested (awaited, so
    /// observers see requests in order; their answers are discarded).
    fn before<'a>(&'a self, request: &'a ApprovalRequest) -> SeamFuture<'a, ()>;

    /// `post_approval_response` with the `choice` (`once`, `session`,
    /// `always`, `deny`) and who made it (`user`, `policy`, `transport`,
    /// `timeout`).
    fn after<'a>(
        &'a self,
        request: &'a ApprovalRequest,
        choice: &'a str,
        decided_by: &'a str,
    ) -> SeamFuture<'a, ()>;

    /// The selected transport's outcome, or `None` when the profile
    /// selects no transport.
    fn transport(&self, request: TransportRequest) -> SeamFuture<'_, Option<TransportOutcome>> {
        let _ = request;
        Box::pin(async { None })
    }
}

/// The wire name of a decision.
#[must_use]
pub fn choice_name(decision: Decision) -> &'static str {
    match decision {
        Decision::Once => "once",
        Decision::Session => "session",
        Decision::Always => "always",
        Decision::Deny => "deny",
    }
}

/// The decision a wire name names.
#[must_use]
pub fn parse_choice(name: &str) -> Option<Decision> {
    match name {
        "once" => Some(Decision::Once),
        "session" => Some(Decision::Session),
        "always" => Some(Decision::Always),
        "deny" => Some(Decision::Deny),
        _ => None,
    }
}

/// Checks a transport's answer against the request it must be bound to:
/// the same id and digest and a choice the request offered.
#[must_use]
pub fn bound_answer(
    request: &TransportRequest,
    id: &str,
    digest: &str,
    choice: &str,
) -> TransportOutcome {
    if id != request.id || digest != request.digest {
        return TransportOutcome::Denied(
            "the transport answered a different (stale or unbound) request".to_owned(),
        );
    }
    match parse_choice(choice) {
        Some(decision) if request.choices.contains(&decision) => {
            TransportOutcome::Decided(decision)
        }
        _ => TransportOutcome::Denied(format!(
            "the transport answered '{choice}', which this request does not offer"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(choices: Vec<Decision>) -> TransportRequest {
        TransportRequest {
            id: "apr_1".to_owned(),
            digest: "sha256:aa".to_owned(),
            request: ApprovalRequest {
                tool: "write_file".to_owned(),
                call_id: "c1".to_owned(),
                subject: Some("AGENTS.md".to_owned()),
                findings: Vec::new(),
                remember: None,
            },
            choices,
            timeout: Duration::from_mins(1),
        }
    }

    #[test]
    fn a_transport_cannot_widen_a_once_only_request() {
        // Spec scenario "Transport cannot widen scope".
        let once_only = request(vec![Decision::Once, Decision::Deny]);
        assert!(matches!(
            bound_answer(&once_only, "apr_1", "sha256:aa", "always"),
            TransportOutcome::Denied(_)
        ));
        assert_eq!(
            bound_answer(&once_only, "apr_1", "sha256:aa", "once"),
            TransportOutcome::Decided(Decision::Once)
        );
    }

    #[test]
    fn an_unbound_answer_is_denied() {
        let req = request(vec![Decision::Once, Decision::Deny]);
        assert!(matches!(
            bound_answer(&req, "apr_2", "sha256:aa", "once"),
            TransportOutcome::Denied(_)
        ));
        assert!(matches!(
            bound_answer(&req, "apr_1", "sha256:bb", "once"),
            TransportOutcome::Denied(_)
        ));
    }
}

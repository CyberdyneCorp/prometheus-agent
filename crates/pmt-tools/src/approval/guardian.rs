//! The smart-mode approval guardian (spec: tool-system, Smart approvals
//! with a denial breaker).
//!
//! With `approvals.mode = "smart"` the gate asks the auxiliary `approval`
//! model about a call before involving a human: `approve` runs it, `deny`
//! refuses it, `escalate` (and any guardian error or timeout) falls back
//! to the human or to the context's typed mode. After
//! `approvals.denial_breaker_threshold` consecutive guardian denials the
//! denial tells the model to stop; any approval resets the count.

use std::sync::atomic::{AtomicU32, Ordering};

use super::gate::ApprovalRequest;
use crate::dispatch::SeamFuture;

/// What the guardian decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardianVerdict {
    /// Run the call.
    Approve,
    /// Refuse it, with the guardian's reason.
    Deny(String),
    /// Let a human (or the context's mode) decide.
    Escalate,
}

/// Judges approval requests (the auxiliary `approval` model).
pub trait Guardian: Send + Sync + std::fmt::Debug {
    /// The verdict on `request`, or why there is none (an error or a
    /// timeout, which escalates).
    fn judge<'a>(
        &'a self,
        request: &'a ApprovalRequest,
    ) -> SeamFuture<'a, Result<GuardianVerdict, String>>;
}

/// The instruction sent to the guardian model.
#[must_use]
pub fn prompt(request: &ApprovalRequest) -> String {
    let subject = request.subject.as_deref().unwrap_or(&request.tool);
    format!(
        "You are the approval guardian of a coding agent. Decide whether the call below may run without asking the user.\n\
         Approve only when it is clearly safe and plainly serves the user's task; deny when it is destructive, exfiltrates data or escalates privileges; escalate when unsure.\n\
         Answer with one line: `approve`, `deny: <reason>` or `escalate`.\n\n\
         Tool: {}\nCall: {subject}\nWhy it was flagged: {}",
        request.tool,
        request.summary()
    )
}

/// Reads the guardian's answer; anything unclear escalates.
#[must_use]
pub fn parse(text: &str) -> GuardianVerdict {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default()
        .trim_matches(['`', '*', '"', '\'']);
    let lower = line.to_ascii_lowercase();
    if lower.starts_with("approve") {
        GuardianVerdict::Approve
    } else if let Some(rest) = lower.strip_prefix("deny") {
        let reason = line[line.len() - rest.len()..]
            .trim_start_matches([':', ' ', '-'])
            .trim();
        GuardianVerdict::Deny(if reason.is_empty() {
            "judged unsafe".to_owned()
        } else {
            reason.to_owned()
        })
    } else {
        GuardianVerdict::Escalate
    }
}

/// Counts consecutive guardian denials of one session.
#[derive(Debug, Default)]
pub struct Breaker {
    denials: AtomicU32,
}

impl Breaker {
    /// Counts one more denial; the note to append once the count reaches
    /// `threshold` (`0` never).
    pub fn deny(&self, threshold: u32) -> Option<String> {
        let count = self.denials.fetch_add(1, Ordering::Relaxed) + 1;
        (threshold > 0 && count >= threshold).then(|| {
            format!(
                "CIRCUIT BREAKER: {count} consecutive commands were blocked. STOP attempting variations of this operation."
            )
        })
    }

    /// An approval (human or guardian) ends the streak.
    pub fn reset(&self) {
        self.denials.store(0, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_parse_and_unclear_ones_escalate() {
        assert_eq!(parse("approve"), GuardianVerdict::Approve);
        assert_eq!(parse("`Approve.`"), GuardianVerdict::Approve);
        assert_eq!(
            parse("deny: wipes the build cache"),
            GuardianVerdict::Deny("wipes the build cache".into())
        );
        assert_eq!(parse("DENY"), GuardianVerdict::Deny("judged unsafe".into()));
        assert_eq!(parse("escalate"), GuardianVerdict::Escalate);
        assert_eq!(parse("maybe?"), GuardianVerdict::Escalate);
        assert_eq!(parse(""), GuardianVerdict::Escalate);
    }

    #[test]
    fn the_breaker_trips_at_the_threshold_and_resets() {
        let breaker = Breaker::default();
        assert_eq!(breaker.deny(3), None);
        assert_eq!(breaker.deny(3), None);
        assert!(
            breaker
                .deny(3)
                .unwrap()
                .starts_with("CIRCUIT BREAKER: 3 consecutive commands")
        );
        breaker.reset();
        assert_eq!(breaker.deny(3), None);
        assert_eq!(Breaker::default().deny(0), None);
    }
}

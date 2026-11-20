//! The gate's side of taint-aware escalation (spec: tool-system,
//! Taint-aware escalation): in a tainted turn, a call that sends data to a
//! target the session has not approved asks a human under
//! `approvals.taint_policy = "ask"`; under `warn` its note only rides along
//! on any prompt the call raises for another reason.

use std::sync::PoisonError;

use pmt_config::schema::TaintPolicy;

use super::super::Finding;
use super::super::rules::PLUGIN_RULE_PREFIX;
use super::super::taint::{self, DETECTOR, RULE_PREFIX};
use super::{ApprovalRequest, CommandGate};
use crate::dispatch::{Escalation, GateRequest, GateVerdict};

/// Removes a call's taint note when its check ends.
pub(super) struct Noted<'a> {
    gate: &'a CommandGate,
    call_id: Option<String>,
}

impl Drop for Noted<'_> {
    fn drop(&mut self) {
        if let Some(call_id) = &self.call_id {
            self.gate.notes().remove(call_id);
        }
    }
}

/// Whether `request` must go to a human (never the smart-mode guardian):
/// it asks because of a tainted turn.
pub(super) fn needs_a_human(request: &ApprovalRequest) -> bool {
    let key = format!("{PLUGIN_RULE_PREFIX}{RULE_PREFIX}");
    request
        .findings
        .iter()
        .any(|f| f.detector.starts_with(&key))
}

impl CommandGate {
    fn notes(&self) -> std::sync::MutexGuard<'_, std::collections::BTreeMap<String, String>> {
        self.taint_notes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// The outbound targets of a call in a tainted turn, with where the
    /// taint came from; nothing when tracking is off.
    fn tainted_targets(&self, request: &GateRequest<'_>) -> Option<(String, Vec<String>)> {
        if self.approvals().taint_policy == TaintPolicy::Off {
            return None;
        }
        let source = request.tainted?;
        let targets = taint::targets(request.tool, request.arguments);
        (!targets.is_empty()).then(|| (source.to_owned(), targets))
    }

    /// Records the taint note of `request` for the prompts it may raise.
    pub(super) fn note_taint(&self, request: &GateRequest<'_>) -> Noted<'_> {
        let Some((source, targets)) = self.tainted_targets(request) else {
            return Noted {
                gate: self,
                call_id: None,
            };
        };
        let note = taint::note(&source, request.tool.name(), &targets.join(", "));
        self.notes().insert(request.call_id.to_owned(), note);
        Noted {
            gate: self,
            call_id: Some(request.call_id.to_owned()),
        }
    }

    /// Adds the taint note of `request`'s call, when it has one, to its
    /// findings.
    pub(super) fn add_taint_note(&self, request: &mut ApprovalRequest) {
        let note = self.notes().get(&request.call_id).cloned();
        if let Some(reason) = note
            && !request.findings.iter().any(|f| f.reason == reason)
        {
            request.findings.push(Finding {
                detector: DETECTOR.to_owned(),
                reason,
                fragment: request.tool.clone(),
            });
        }
    }

    /// Under `ask`: a human approval for each outbound target of a call in
    /// a tainted turn that the session has not approved yet (`session` or
    /// `always` remembers the target).
    pub(super) async fn check_taint(&self, request: &GateRequest<'_>) -> GateVerdict {
        if self.approvals().taint_policy != TaintPolicy::Ask {
            return GateVerdict::Allow;
        }
        let Some((source, targets)) = self.tainted_targets(request) else {
            return GateVerdict::Allow;
        };
        for target in targets {
            let escalation = Escalation::new(
                format!("{RULE_PREFIX}{target}"),
                taint::note(&source, request.tool.name(), &target),
            );
            let verdict = self.check_escalation(request, &escalation).await;
            if verdict != GateVerdict::Allow {
                return verdict;
            }
        }
        GateVerdict::Allow
    }
}

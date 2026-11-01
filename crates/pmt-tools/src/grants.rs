//! The grant gate: a capability a subject (the session, a plugin, an MCP
//! server, a skill) needs but does not hold is asked of the human, and the
//! answer is applied (spec: security-boundaries, Grant prompts fail closed;
//! Grants are stored, reviewable and revocable).
//!
//! `once` allows this use only, `session` adds a session-only grant,
//! `always` records the grant in `grants.toml` (never `config.toml`), and
//! `deny`, a dismissed prompt, a detached surface, the timeout or an
//! unattended context all deny with `capability not granted`.

use std::sync::Arc;

use pmt_config::grants::{SessionGrants, Subject};
use pmt_core::capability::CapabilitySet;
use pmt_core::error::{Error, codes};
use pmt_core::human::{Channel, Unanswered};
use serde::{Deserialize, Serialize};

/// What the human is asked: the subject, the exact capabilities it lacks
/// and why it wants them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantRequest {
    /// Who asks.
    pub subject: Subject,
    /// The capabilities it lacks.
    pub capabilities: CapabilitySet,
    /// Why (shown verbatim).
    pub reason: String,
}

/// A human's answer (the `decision` of `approval.respond`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantAnswer {
    /// Allow this use only.
    Once,
    /// Allow until the session ends.
    Session,
    /// Allow now and in later sessions (recorded in `grants.toml`).
    Always,
    /// Refuse.
    Deny,
}

/// Checks capabilities against a session's grants and asks for the rest.
#[derive(Debug)]
pub struct GrantGate {
    grants: Arc<SessionGrants>,
    channel: Channel<GrantRequest, GrantAnswer>,
    surface: String,
}

impl GrantGate {
    /// A gate over `grants` that asks through `channel`; `surface` names
    /// the interface answers come from (`cli`, `tui`, `desktop`...), as
    /// recorded with each permanent grant.
    #[must_use]
    pub fn new(
        grants: Arc<SessionGrants>,
        channel: Channel<GrantRequest, GrantAnswer>,
        surface: &str,
    ) -> Self {
        Self {
            grants,
            channel,
            surface: surface.to_owned(),
        }
    }

    /// The session's grants.
    #[must_use]
    pub fn grants(&self) -> &SessionGrants {
        &self.grants
    }

    /// `Ok` when `subject` holds every capability of `needed`, asking the
    /// human for the missing ones.
    ///
    /// # Errors
    ///
    /// `grants.denied` (exit 77) `capability not granted: <caps>: <why>`
    /// on any answer but once/session/always; `grants.store` when an
    /// `always` answer cannot be recorded.
    pub async fn require(
        &self,
        subject: &Subject,
        needed: &CapabilitySet,
        reason: &str,
    ) -> Result<(), Error> {
        let missing: CapabilitySet = self
            .grants
            .capabilities(subject)?
            .missing(needed)
            .into_iter()
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        let request = GrantRequest {
            subject: subject.clone(),
            capabilities: missing.clone(),
            reason: reason.to_owned(),
        };
        match self.channel.ask(request).await {
            Ok(GrantAnswer::Once) => Ok(()),
            Ok(GrantAnswer::Session) => {
                self.grants.grant_for_session(subject, &missing);
                Ok(())
            }
            Ok(GrantAnswer::Always) => {
                let file = self.grants.persistent().file();
                file.add(subject, &missing, &self.surface).map(drop)
            }
            Ok(GrantAnswer::Deny) => Err(denied(&missing, "denied by the user")),
            Err(unanswered) => Err(denied(&missing, &unanswered_reason(unanswered))),
        }
    }
}

fn unanswered_reason(unanswered: Unanswered) -> String {
    match unanswered {
        Unanswered::TimedOut(after) => format!(
            "no answer within {} s (approvals.timeout_secs)",
            after.as_secs()
        ),
        other => other.to_string(),
    }
}

fn denied(capabilities: &CapabilitySet, why: &str) -> Error {
    Error::new(
        &codes::GRANTS_DENIED,
        format!(
            "capability not granted: {}: {why}",
            capabilities.to_strings().join(", ")
        ),
    )
}

#[cfg(test)]
mod tests;

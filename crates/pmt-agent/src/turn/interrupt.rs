//! Stopping a running turn (spec: agent-turn-loop, Interrupts; Single
//! active turn per session): the first issuer wins.
//!
//! A hard stop cancels the turn's model call, its tool workers and the
//! delegated children that hold [`Interrupt::token`] (or a child of it),
//! and discards undelivered steers. A soft stop cancels only the model call
//! in flight (it has no side effects): a running tool batch finishes, the
//! turn ends at the next safe point and undelivered steers wait for the
//! next turn.

use std::sync::{Arc, Mutex, PoisonError};

use pmt_core::cancel::CancelToken;

/// Who stopped the turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Issuer {
    /// The user (Ctrl-C, `/stop`, a new message in interrupt mode).
    User,
    /// The system, with a reason (`lease_lost`, `shutdown`...).
    System(String),
}

impl Issuer {
    /// The issuer's name in results and events.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::User => "user",
            Self::System(reason) => reason,
        }
    }
}

/// A handle that stops one turn; clone it before passing the options in.
#[derive(Debug, Clone)]
pub struct Interrupt {
    /// Cancelled by a hard stop (and by the parent token).
    hard: CancelToken,
    /// Cancelled by any stop (a child of `hard`).
    any: CancelToken,
    issuer: Arc<Mutex<Option<Issuer>>>,
}

impl Default for Interrupt {
    fn default() -> Self {
        Self::child_of(&CancelToken::new())
    }
}

impl Interrupt {
    /// A handle whose token is a child of `parent` (session shutdown
    /// cancels the turn too, as a hard stop).
    #[must_use]
    pub fn child_of(parent: &CancelToken) -> Self {
        let hard = parent.child();
        Self {
            any: hard.child(),
            hard,
            issuer: Arc::default(),
        }
    }

    /// Hard stop: cancels running tools and children; a second stop keeps
    /// the first issuer.
    pub fn stop(&self, issuer: Issuer) {
        self.record(issuer);
        self.hard.cancel();
    }

    /// Soft stop: the running tool batch finishes, then the turn ends.
    pub fn stop_soft(&self, issuer: Issuer) {
        self.record(issuer);
        self.any.cancel();
    }

    fn record(&self, issuer: Issuer) {
        self.issuer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_or_insert(issuer);
    }

    /// Who stopped the turn; a cancelled parent token without an issuer
    /// is the system shutting down.
    #[must_use]
    pub fn issuer(&self) -> Option<Issuer> {
        if !self.any.is_cancelled() {
            return None;
        }
        let issuer = self
            .issuer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        Some(issuer.unwrap_or_else(|| Issuer::System("shutdown".to_owned())))
    }

    /// Whether the turn was stopped hard.
    #[must_use]
    pub fn is_hard(&self) -> bool {
        self.hard.is_cancelled()
    }

    /// The token a hard stop cancels: tool workers and delegated children
    /// run under it (or a child of it).
    #[must_use]
    pub fn token(&self) -> &CancelToken {
        &self.hard
    }

    /// The token any stop cancels (the model call in flight).
    pub(crate) fn any(&self) -> &CancelToken {
        &self.any
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_soft_stop_leaves_tools_running_and_a_hard_stop_cancels_them() {
        let soft = Interrupt::default();
        soft.stop_soft(Issuer::User);
        assert_eq!(soft.issuer(), Some(Issuer::User));
        assert!(!soft.is_hard() && !soft.token().is_cancelled());
        soft.stop(Issuer::System("shutdown".into()));
        assert!(soft.is_hard());
        assert_eq!(soft.issuer(), Some(Issuer::User), "first issuer wins");

        let parent = CancelToken::new();
        let child = Interrupt::child_of(&parent);
        parent.cancel();
        assert!(child.is_hard());
        assert_eq!(child.issuer().unwrap().name(), "shutdown");
    }
}

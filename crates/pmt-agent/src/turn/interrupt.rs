//! Stopping a running turn (spec: agent-turn-loop, Interrupts; Single
//! active turn per session): the first issuer wins and the turn's cancel
//! token stops its model call and tool batch.

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
#[derive(Debug, Clone, Default)]
pub struct Interrupt {
    token: CancelToken,
    issuer: Arc<Mutex<Option<Issuer>>>,
}

impl Interrupt {
    /// A handle whose token is a child of `parent` (session shutdown
    /// cancels the turn too).
    #[must_use]
    pub fn child_of(parent: &CancelToken) -> Self {
        Self {
            token: parent.child(),
            issuer: Arc::default(),
        }
    }

    /// Stops the turn; a second call keeps the first issuer.
    pub fn stop(&self, issuer: Issuer) {
        self.issuer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_or_insert(issuer);
        self.token.cancel();
    }

    /// Who stopped the turn; a cancelled parent token without an issuer
    /// is the system shutting down.
    #[must_use]
    pub fn issuer(&self) -> Option<Issuer> {
        if !self.token.is_cancelled() {
            return None;
        }
        let issuer = self
            .issuer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        Some(issuer.unwrap_or_else(|| Issuer::System("shutdown".to_owned())))
    }

    /// The turn's cancel token.
    #[must_use]
    pub fn token(&self) -> &CancelToken {
        &self.token
    }
}

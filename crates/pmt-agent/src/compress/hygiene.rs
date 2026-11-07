//! Gateway session hygiene (spec: context-compression, Gateway session
//! hygiene).
//!
//! Before the gateway hands an inbound message to the agent, a session
//! whose last known context reaches 85% of the main model's window, or
//! whose transcript reaches 5,000 messages, is compacted first (trigger
//! `gateway_hygiene`), so a conversation that grew overnight does not
//! overflow on the next message. The context size is the provider's last
//! reported count, else the persisted usage of the session's last main
//! model call, else a rough estimate. Sessions with fewer than 4 messages
//! are left alone.

/// The share of the window, in percent, at which hygiene compacts.
pub const WINDOW_PERCENT: u64 = 85;
/// The message count at which hygiene compacts.
pub const MAX_MESSAGES: usize = 5_000;
/// Sessions with fewer messages are never compacted by hygiene.
pub const MIN_MESSAGES: usize = 4;

/// Where the context size came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Known {
    /// The provider's count for the latest request of this agent.
    Reported(u64),
    /// The usage persisted for the session's last main model call.
    Persisted(u64),
    /// A rough estimate of the transcript.
    Estimated(u64),
}

impl Known {
    /// The tokens.
    #[must_use]
    pub const fn tokens(self) -> u64 {
        match self {
            Self::Reported(t) | Self::Persisted(t) | Self::Estimated(t) => t,
        }
    }
}

/// Whether a session of `messages` active rows whose context is `known`,
/// on a model with `window` tokens, is compacted before the next turn.
#[must_use]
pub fn needed(known: Known, window: u64, messages: usize) -> bool {
    if messages < MIN_MESSAGES {
        return false;
    }
    known.tokens().saturating_mul(100) >= window.saturating_mul(WINDOW_PERCENT)
        || messages >= MAX_MESSAGES
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hygiene_table() {
        let window = 200_000;
        assert!(needed(Known::Reported(180_000), window, 40));
        assert!(needed(Known::Persisted(170_000), window, 40));
        assert!(!needed(Known::Estimated(169_999), window, 40));
        assert!(needed(Known::Estimated(10), window, 5_000));
        assert!(!needed(Known::Reported(199_000), window, 3));
    }
}

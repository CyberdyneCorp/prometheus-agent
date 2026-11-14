//! One-time login tickets (spec: web-dashboard, Loopback authentication
//! with an `HttpOnly` session cookie): minted only over the daemon's
//! owner-only control channel (`prometheus dashboard`), valid 60 s, single
//! use. A redeemed or expired ticket is refused the same way.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use tokio::time::Instant;

use super::secret::{hash, random_hex};

/// How long a login ticket stays valid.
pub(crate) const TTL: Duration = Duration::from_mins(1);

/// The listener's live login tickets, by hash.
#[derive(Debug, Default)]
pub(crate) struct Tickets {
    live: Mutex<HashMap<String, Instant>>,
}

impl Tickets {
    /// A new ticket.
    pub(crate) fn mint(&self) -> String {
        let ticket = random_hex(32);
        let mut live = self.live.lock().expect("tickets poisoned");
        let now = Instant::now();
        live.retain(|_, issued| now.duration_since(*issued) < TTL);
        live.insert(hash(&ticket), now);
        ticket
    }

    /// Uses `ticket`: true once, within its TTL.
    pub(crate) fn redeem(&self, ticket: &str) -> bool {
        let mut live = self.live.lock().expect("tickets poisoned");
        live.remove(&hash(ticket))
            .is_some_and(|issued| issued.elapsed() < TTL)
    }
}

#[cfg(test)]
mod tests {
    use super::{TTL, Tickets};

    /// Scenario: Reused login link (the ticket side).
    #[tokio::test(start_paused = true)]
    async fn a_ticket_works_once_and_only_within_a_minute() {
        let tickets = Tickets::default();
        let first = tickets.mint();
        assert!(tickets.redeem(&first));
        assert!(!tickets.redeem(&first));
        assert!(!tickets.redeem("not-a-ticket"));
        let late = tickets.mint();
        tokio::time::advance(TTL).await;
        assert!(!tickets.redeem(&late));
    }
}

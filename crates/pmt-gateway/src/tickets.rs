//! Single-use RPC tickets (spec: workspace-architecture, Ticket-authenticated
//! loopback attachment): granted over the control channel, valid 30 s,
//! bound to this daemon instance, its profiles and one purpose. Only a hash
//! of each ticket is kept, so not even a memory dump of the daemon replays
//! one; a used hash is remembered so a replay is told from a typo.

use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::sync::Mutex;
use std::time::Duration;

// tokio's clock, so tests can advance it past the expiry.
use tokio::time::Instant;

use pmt_core::error::{Error, codes};
use pmt_rpc::control::{Purpose, Ticket};
use sha2::{Digest as _, Sha256};

/// How long a ticket stays valid.
pub const TTL: Duration = Duration::from_secs(30);

/// Used ticket hashes remembered to recognise replays.
const USED_KEPT: usize = 1024;

/// What a redeemed ticket grants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    /// The profiles the connection may use.
    pub profiles: Vec<String>,
    /// What the connection is for.
    pub purpose: Purpose,
}

/// The outcome of presenting a ticket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Redeemed {
    /// Valid: the ticket is now used.
    Granted(Grant),
    /// Never issued by this daemon instance.
    Unknown,
    /// Issued, but older than [`TTL`].
    Expired,
    /// Already used by another connection.
    Reused,
}

#[derive(Debug, Default)]
struct Book {
    live: HashMap<String, (Grant, Instant)>,
    used: VecDeque<String>,
}

/// The tickets of one daemon instance.
#[derive(Debug, Default)]
pub struct TicketBook {
    book: Mutex<Book>,
}

fn hash(ticket: &str) -> String {
    hex(&Sha256::digest(ticket.as_bytes()))
}

/// Lowercase hex of `bytes`.
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

/// `bytes` random bytes as lowercase hex.
pub(crate) fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0_u8; bytes];
    // The OS generator failing is not recoverable for a credential.
    getrandom::fill(&mut buf).expect("the OS random generator is available");
    hex(&buf)
}

impl TicketBook {
    /// Issues a ticket for `profiles` (each one of `served`).
    ///
    /// # Errors
    ///
    /// `rpc.ticket` for no profile, several profiles for a purpose other
    /// than `profile-admin`, or a profile this daemon does not serve.
    ///
    /// # Panics
    ///
    /// When the ticket book's lock is poisoned (a panic while holding it).
    pub fn issue(
        &self,
        purpose: Purpose,
        profiles: Vec<String>,
        served: &[String],
    ) -> Result<Ticket, Error> {
        let several = profiles.len() > 1 && purpose != Purpose::ProfileAdmin;
        if profiles.is_empty() || several {
            return Err(Error::new(
                &codes::RPC_TICKET,
                format!(
                    "a '{}' ticket grants exactly one profile (only 'profile-admin' may grant several)",
                    purpose.as_str()
                ),
            ));
        }
        if let Some(unknown) = profiles.iter().find(|p| !served.contains(p)) {
            return Err(Error::new(
                &codes::RPC_TICKET,
                format!("this daemon does not serve profile '{unknown}'"),
            ));
        }
        let ticket = random_hex(32);
        let mut book = self.book.lock().expect("ticket book poisoned");
        let now = Instant::now();
        book.live
            .retain(|_, (_, issued)| now.duration_since(*issued) < TTL);
        book.live
            .insert(hash(&ticket), (Grant { profiles, purpose }, now));
        Ok(Ticket {
            ticket,
            expires_in_secs: TTL.as_secs(),
        })
    }

    /// Redeems `ticket`, consuming it.
    ///
    /// # Panics
    ///
    /// When the ticket book's lock is poisoned (a panic while holding it).
    pub fn redeem(&self, ticket: &str) -> Redeemed {
        let key = hash(ticket);
        let mut book = self.book.lock().expect("ticket book poisoned");
        if book.used.contains(&key) {
            return Redeemed::Reused;
        }
        let Some((grant, issued)) = book.live.remove(&key) else {
            return Redeemed::Unknown;
        };
        book.used.push_back(key);
        if book.used.len() > USED_KEPT {
            book.used.pop_front();
        }
        if issued.elapsed() >= TTL {
            return Redeemed::Expired;
        }
        Redeemed::Granted(grant)
    }
}

#[cfg(test)]
mod tests {
    use pmt_rpc::control::Purpose;

    use super::{Redeemed, TicketBook};

    fn served() -> Vec<String> {
        vec!["default".into(), "work".into()]
    }

    #[test]
    fn a_ticket_is_single_use() {
        let book = TicketBook::default();
        let ticket = book
            .issue(Purpose::Interactive, vec!["work".into()], &served())
            .unwrap();
        assert_eq!(ticket.expires_in_secs, 30);
        assert!(
            matches!(book.redeem(&ticket.ticket), Redeemed::Granted(g) if g.profiles == ["work"])
        );
        assert_eq!(book.redeem(&ticket.ticket), Redeemed::Reused);
        assert_eq!(book.redeem("never-issued"), Redeemed::Unknown);
    }

    #[tokio::test(start_paused = true)]
    async fn a_ticket_expires_after_30_seconds() {
        let book = TicketBook::default();
        let ticket = book
            .issue(Purpose::Interactive, vec!["work".into()], &served())
            .unwrap();
        tokio::time::advance(std::time::Duration::from_secs(31)).await;
        assert_eq!(book.redeem(&ticket.ticket), Redeemed::Expired);
    }

    #[test]
    fn only_profile_admin_grants_several_profiles() {
        let book = TicketBook::default();
        assert!(
            book.issue(Purpose::Interactive, served(), &served())
                .is_err()
        );
        assert!(book.issue(Purpose::Interactive, vec![], &served()).is_err());
        assert!(
            book.issue(Purpose::Interactive, vec!["other".into()], &served())
                .is_err()
        );
        assert!(
            book.issue(Purpose::ProfileAdmin, served(), &served())
                .is_ok()
        );
    }
}

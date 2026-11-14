//! One-use dial tickets (spec: desktop-app, One-use dial tickets never reach
//! the webview's storage). Every WebSocket dial gets its own ticket from
//! the daemon's owner-only control channel, recorded here against the
//! window and path that asked for it until the window reports the dial
//! settled or the ticket expires. At most [`MAX_PENDING`] are outstanding.
//! The ticket itself is never stored: the record keeps only who asked.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Outstanding dials at most.
pub const MAX_PENDING: usize = 100;
/// A ticket's lifetime (the daemon's ticket TTL).
pub const TTL: Duration = Duration::from_secs(30);

/// What a dial is for: the two purposes a desktop window may ask for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DialPurpose {
    /// The window's client protocol connection.
    #[default]
    Interactive,
    /// An HTTP surface of the daemon.
    Http,
}

impl From<DialPurpose> for pmt_rpc::control::Purpose {
    fn from(p: DialPurpose) -> Self {
        match p {
            DialPurpose::Interactive => Self::Interactive,
            DialPurpose::Http => Self::Http,
        }
    }
}

/// Why a dial was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DialError {
    /// [`MAX_PENDING`] dials are outstanding.
    TooMany,
    /// No such pending dial for this window.
    Unknown,
}

impl std::fmt::Display for DialError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::TooMany => "Too many pending gateway dials",
            Self::Unknown => "no such pending gateway dial",
        })
    }
}

impl std::error::Error for DialError {}

#[derive(Debug)]
struct Pending {
    window: String,
    path: String,
    purpose: DialPurpose,
    expires: Instant,
}

/// The outstanding dials of the shell.
#[derive(Debug, Default)]
pub struct Dials {
    pending: Mutex<HashMap<String, Pending>>,
    next: Mutex<u64>,
}

impl Dials {
    /// Reserves a dial for `window` to `path` before its ticket is asked
    /// for, so concurrent dials count against the cap; returns its id.
    ///
    /// # Errors
    ///
    /// [`DialError::TooMany`].
    ///
    /// # Panics
    ///
    /// When the lock was poisoned by a panic elsewhere.
    pub fn reserve(
        &self,
        window: &str,
        path: &str,
        purpose: DialPurpose,
        now: Instant,
    ) -> Result<String, DialError> {
        let mut pending = self.pending.lock().expect("dials poisoned");
        pending.retain(|_, p| p.expires > now);
        if pending.len() >= MAX_PENDING {
            return Err(DialError::TooMany);
        }
        let id = {
            let mut next = self.next.lock().expect("dials poisoned");
            *next += 1;
            format!("dial-{next}")
        };
        pending.insert(
            id.clone(),
            Pending {
                window: window.to_owned(),
                path: path.to_owned(),
                purpose,
                expires: now + TTL,
            },
        );
        Ok(id)
    }

    /// Ends dial `id` of `window` (opened, failed, or its ticket could not
    /// be obtained). Another window's dial is not this window's to end.
    ///
    /// # Errors
    ///
    /// [`DialError::Unknown`].
    ///
    /// # Panics
    ///
    /// When the lock was poisoned by a panic elsewhere.
    pub fn settle(&self, window: &str, id: &str) -> Result<(), DialError> {
        let mut pending = self.pending.lock().expect("dials poisoned");
        match pending.get(id) {
            Some(p) if p.window == window => {
                pending.remove(id);
                Ok(())
            }
            _ => Err(DialError::Unknown),
        }
    }

    /// The `(window, path, purpose)` a pending dial is bound to.
    ///
    /// # Panics
    ///
    /// When the lock was poisoned by a panic elsewhere.
    #[must_use]
    pub fn binding(&self, id: &str) -> Option<(String, String, DialPurpose)> {
        let pending = self.pending.lock().expect("dials poisoned");
        pending
            .get(id)
            .map(|p| (p.window.clone(), p.path.clone(), p.purpose))
    }

    /// Outstanding dials (expired ones excluded).
    ///
    /// # Panics
    ///
    /// When the lock was poisoned by a panic elsewhere.
    #[must_use]
    pub fn outstanding(&self, now: Instant) -> usize {
        let pending = self.pending.lock().expect("dials poisoned");
        pending.values().filter(|p| p.expires > now).count()
    }
}

/// A prepared dial handed to the window: the ticket rides only inside
/// `protocols`, which `Debug` never prints.
#[derive(Clone, Serialize)]
pub struct Dial {
    /// Settle it with this id.
    pub dial: String,
    /// The WebSocket URL.
    pub url: String,
    /// `prometheus-rpc.v<major>` and `prometheus-ticket.<ticket>`.
    pub protocols: Vec<String>,
}

impl std::fmt::Debug for Dial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dial")
            .field("dial", &self.dial)
            .field("url", &self.url)
            .field("protocols", &"<redacted>")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::{DialError, DialPurpose, Dials, MAX_PENDING, TTL};

    #[test]
    fn a_hundred_pending_dials_refuse_the_next() {
        let dials = Dials::default();
        let now = Instant::now();
        for _ in 0..MAX_PENDING {
            dials
                .reserve("main", "/rpc", DialPurpose::Interactive, now)
                .unwrap();
        }
        let refused = dials.reserve("main", "/rpc", DialPurpose::Interactive, now);
        assert_eq!(refused, Err(DialError::TooMany));
        assert_eq!(
            DialError::TooMany.to_string(),
            "Too many pending gateway dials"
        );
        // Expired tickets no longer count.
        assert!(
            dials
                .reserve("main", "/rpc", DialPurpose::Interactive, now + TTL)
                .is_ok()
        );
    }

    #[test]
    fn a_dial_is_bound_to_the_window_that_asked() {
        let dials = Dials::default();
        let id = dials
            .reserve("main", "/rpc", DialPurpose::Http, Instant::now())
            .unwrap();
        assert_eq!(
            dials.binding(&id),
            Some(("main".into(), "/rpc".into(), DialPurpose::Http))
        );
        assert_eq!(dials.settle("other", &id), Err(DialError::Unknown));
        assert_eq!(dials.settle("main", &id), Ok(()));
        assert_eq!(dials.settle("main", &id), Err(DialError::Unknown));
    }

    #[test]
    fn a_dial_never_prints_its_ticket() {
        let dial = super::Dial {
            dial: "dial-1".into(),
            url: "ws://127.0.0.1:1/rpc".into(),
            protocols: vec!["prometheus-ticket.secret".into()],
        };
        assert!(!format!("{dial:?}").contains("secret"));
    }
}

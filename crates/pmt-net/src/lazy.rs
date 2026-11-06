//! An HTTP client built on first use.
//!
//! Building a client starts the platform certificate verifier, which on
//! Linux loads and parses every root of the system store (about a
//! millisecond or two each time). Session start wires several components
//! that may never send a request (runtime installers, cloud browser
//! providers); they hold a [`LazyClient`] so a chat start does not pay for
//! clients nobody uses (spec: workspace-architecture, Startup time budget).

use std::sync::OnceLock;

use pmt_core::error::Error;

/// How a [`LazyClient`] builds its client.
type Build = Box<dyn Fn() -> Result<reqwest::Client, Error> + Send + Sync>;

/// A client built by the first [`LazyClient::get`]; a failed build is
/// retried by the next call.
pub struct LazyClient {
    build: Build,
    client: OnceLock<reqwest::Client>,
}

impl std::fmt::Debug for LazyClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LazyClient")
            .field("built", &self.client.get().is_some())
            .finish_non_exhaustive()
    }
}

impl LazyClient {
    /// A client `build` makes on first use (typically
    /// `crate::client_builder` plus the caller's timeouts).
    #[must_use]
    pub fn new(build: impl Fn() -> Result<reqwest::Client, Error> + Send + Sync + 'static) -> Self {
        Self {
            build: Box::new(build),
            client: OnceLock::new(),
        }
    }

    /// A client that is already built (tests, callers holding one).
    #[must_use]
    pub fn ready(client: reqwest::Client) -> Self {
        Self {
            build: Box::new(unreachable_build),
            client: OnceLock::from(client),
        }
    }

    /// The client, built now when this is the first use.
    ///
    /// # Errors
    ///
    /// What the build returns (`net.client_init`, `net.extra_ca`).
    pub fn get(&self) -> Result<&reqwest::Client, Error> {
        if let Some(client) = self.client.get() {
            return Ok(client);
        }
        // Two first uses racing may both build; one client is kept.
        let built = (self.build)()?;
        Ok(self.client.get_or_init(|| built))
    }
}

/// The build of a [`LazyClient::ready`] client, which is never called: the
/// client is set before any `get`.
fn unreachable_build() -> Result<reqwest::Client, Error> {
    Err(Error::new(
        &pmt_core::error::codes::NET_CLIENT_INIT,
        "the HTTP client was not provided",
    ))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    fn counting() -> (LazyClient, Arc<AtomicUsize>) {
        let builds = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&builds);
        let lazy = LazyClient::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            crate::client(&crate::OutboundPolicy::default())
        });
        (lazy, builds)
    }

    #[test]
    fn nothing_is_built_until_the_first_use_and_then_only_once() {
        let (lazy, builds) = counting();
        assert_eq!(builds.load(Ordering::SeqCst), 0);
        lazy.get().unwrap();
        lazy.get().unwrap();
        assert_eq!(builds.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_failed_build_is_reported_and_retried() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&attempts);
        let lazy = LazyClient::new(move || {
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                crate::client(&crate::OutboundPolicy {
                    proxy: Some("not a proxy url".to_owned()),
                    ..crate::OutboundPolicy::default()
                })
            } else {
                crate::client(&crate::OutboundPolicy::default())
            }
        });
        let err = lazy.get().unwrap_err();
        assert!(err.message().contains("network.proxy"), "{}", err.message());
        lazy.get().unwrap();
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_ready_client_is_used_as_given() {
        let lazy = LazyClient::ready(crate::client(&crate::OutboundPolicy::default()).unwrap());
        lazy.get().unwrap();
    }
}

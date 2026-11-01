//! Limits read at each use rather than fixed when a session is built, so
//! a long-running process that reloads its configuration applies the
//! `live` keys to sessions already running (spec: configuration-and-
//! profiles, Live reload with apply classes).

use std::sync::Arc;

/// A size limit as it is now.
pub type Limit = Arc<dyn Fn() -> usize + Send + Sync>;

/// A limit that never changes.
#[must_use]
pub fn fixed(value: usize) -> Limit {
    Arc::new(move || value)
}

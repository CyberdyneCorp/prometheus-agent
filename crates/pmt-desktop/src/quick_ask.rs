//! The quick-ask session choice (spec: desktop-app, Global hotkey
//! quick-ask): a question asked through the hotkey goes to the previous
//! quick-ask session of the same profile when that session was active
//! within `desktop.quick_ask.reuse_secs`, else to a new one.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The last quick-ask session.
#[derive(Debug, Default)]
pub struct QuickAsks {
    last: Mutex<Option<(String, String, Instant)>>,
}

impl QuickAsks {
    /// The session to ask in for `profile` at `now`; `None` means create
    /// one.
    pub fn target(&self, profile: &str, now: Instant, reuse: Duration) -> Option<String> {
        let last = self.lock();
        last.as_ref()
            .filter(|(p, _, at)| p == profile && now.saturating_duration_since(*at) <= reuse)
            .map(|(_, session, _)| session.clone())
    }

    /// `session` of `profile` was asked in at `now`.
    pub fn used(&self, profile: &str, session: &str, now: Instant) {
        *self.lock() = Some((profile.to_owned(), session.to_owned(), now));
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<(String, String, Instant)>> {
        self.last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::QuickAsks;

    #[test]
    fn a_quick_follow_up_reuses_the_session() {
        let asks = QuickAsks::default();
        let start = Instant::now();
        let reuse = Duration::from_mins(15);
        assert_eq!(asks.target("default", start, reuse), None);
        asks.used("default", "ses_A", start);
        let five_minutes = start + Duration::from_mins(5);
        assert_eq!(
            asks.target("default", five_minutes, reuse).as_deref(),
            Some("ses_A")
        );
        assert_eq!(
            asks.target("work", five_minutes, reuse),
            None,
            "another profile"
        );
        let later = start + Duration::from_secs(901);
        assert_eq!(asks.target("default", later, reuse), None, "too long ago");
    }
}

//! The scheduler's clock: the system clock in production, a settable one
//! in tests (spec: scheduled-jobs, verified with a fake clock).

use std::sync::Mutex;

use jiff::{SignedDuration, Timestamp};

/// Where "now" comes from.
pub trait Clock: Send + Sync {
    /// The current instant.
    fn now(&self) -> Timestamp;
}

/// The system clock.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        Timestamp::now()
    }
}

/// A clock that moves only when told to.
#[derive(Debug)]
pub struct FakeClock(Mutex<Timestamp>);

impl FakeClock {
    /// A clock showing `now`.
    #[must_use]
    pub fn new(now: Timestamp) -> Self {
        Self(Mutex::new(now))
    }

    /// Shows `now` from now on.
    pub fn set(&self, now: Timestamp) {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = now;
    }

    /// Moves forward by `by`.
    pub fn advance(&self, by: SignedDuration) {
        let mut now = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *now = now.checked_add(by).unwrap_or(*now);
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Timestamp {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

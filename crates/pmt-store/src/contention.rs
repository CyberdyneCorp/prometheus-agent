//! Write contention policy (spec: session-storage, Write contention
//! policy): a write that finds another process holding the lock retries
//! its whole transaction with random jitter until its class's time budget
//! runs out, then fails with `store.busy` (exit 75).

use std::time::{Duration, Instant};

use pmt_core::error::{Error, codes};

/// What a write is for; each class has its own time budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteClass {
    /// Session records, titles, leases, maintenance: 20 s.
    Metadata,
    /// Transcript rows (appends, compaction, rewind): 60 s.
    Transcript,
    /// Activity heartbeats, which must never stall a turn: 0.5 s.
    Activity,
}

impl WriteClass {
    /// How long the write may wait for other writers in total.
    #[must_use]
    pub const fn budget(self) -> Duration {
        match self {
            Self::Metadata => Duration::from_secs(20),
            Self::Transcript => Duration::from_mins(1),
            Self::Activity => Duration::from_millis(500),
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Metadata => "metadata",
            Self::Transcript => "transcript",
            Self::Activity => "activity",
        }
    }
}

/// One write's retry clock.
pub(crate) struct Attempts {
    class: WriteClass,
    started: Instant,
}

impl Attempts {
    pub(crate) fn start(class: WriteClass) -> Self {
        Self {
            class,
            started: Instant::now(),
        }
    }

    /// Budget left, never zero (SQLite treats a zero timeout as "no wait").
    pub(crate) fn remaining(&self) -> Duration {
        self.class
            .budget()
            .saturating_sub(self.started.elapsed())
            .max(Duration::from_millis(1))
    }

    /// After a busy attempt: sleeps a jittered pause and returns `Ok`, or
    /// returns `store.busy` when the budget is spent.
    pub(crate) fn back_off(&self) -> Result<(), Error> {
        let elapsed = self.started.elapsed();
        if elapsed >= self.class.budget() {
            return Err(self.exhausted());
        }
        let pause = jitter(elapsed).min(self.class.budget().saturating_sub(elapsed));
        std::thread::sleep(pause);
        Ok(())
    }

    fn exhausted(&self) -> Error {
        let budget = self.class.budget();
        Error::new(
            &codes::STORE_BUSY,
            format!(
                "state.db stayed locked by another writer for the whole {} write budget ({} ms)",
                self.class.name(),
                budget.as_millis()
            ),
        )
        .with_hint("another Prometheus process is writing to this profile; retry when it is idle")
    }
}

/// 20-150 ms during the first 2 s of waiting, then 250 ms-1 s.
fn jitter(elapsed: Duration) -> Duration {
    let (low, high) = if elapsed < Duration::from_secs(2) {
        (20, 150)
    } else {
        (250, 1000)
    };
    let span = high - low + 1;
    // ULID generation draws from the OS-seeded RNG; no extra dependency.
    let random = u64::try_from(ulid::Ulid::generate().random() % u128::from(span)).unwrap_or(0);
    Duration::from_millis(low + random)
}

/// Whether `err` is a lock held by another connection.
pub(crate) fn is_busy(err: &Error) -> bool {
    err.code().code == codes::STORE_BUSY.code
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{WriteClass, jitter};

    #[test]
    fn jitter_stays_in_its_window() {
        for _ in 0..200 {
            let early = jitter(Duration::from_millis(100));
            assert!((20..=150).contains(&early.as_millis()), "{early:?}");
            let late = jitter(Duration::from_secs(3));
            assert!((250..=1000).contains(&late.as_millis()), "{late:?}");
        }
    }

    #[test]
    fn budgets_follow_the_spec() {
        assert_eq!(WriteClass::Metadata.budget(), Duration::from_secs(20));
        assert_eq!(WriteClass::Transcript.budget(), Duration::from_mins(1));
        assert_eq!(WriteClass::Activity.budget(), Duration::from_millis(500));
    }
}

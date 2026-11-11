//! The concurrent run cap (spec: api-server, Concurrent run cap): each
//! profile runs at most `api_server.max_concurrent_runs` turns started by
//! the API at once (`0`: no cap). A run holds its [`Permit`] until its
//! work actually exits, including while it is stopping.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::http::{Reply, error, with_header};

/// Running API turns per profile.
#[derive(Debug, Default)]
pub(crate) struct Limits {
    running: Mutex<HashMap<String, Arc<()>>>,
}

/// One running API turn; dropping it frees its slot.
#[derive(Debug)]
pub(crate) struct Permit {
    _slot: Arc<()>,
}

impl Limits {
    /// A slot of `profile` under `max` (`0`: unlimited), or the 429 that
    /// refuses the request.
    pub(crate) fn acquire(&self, profile: &str, max: u32) -> Result<Permit, Reply> {
        let mut running = self.running.lock().expect("limits poisoned");
        let counter = running.entry(profile.to_owned()).or_default();
        // Every live permit holds one clone of the profile's counter.
        let active = Arc::strong_count(counter) - 1;
        if max > 0 && active >= max as usize {
            let refused = error(
                429,
                "rate_limit_error",
                "too_many_runs",
                &format!("Too many concurrent runs (max {max})"),
            );
            return Err(with_header(refused, "retry-after", "5"));
        }
        Ok(Permit {
            _slot: Arc::clone(counter),
        })
    }

    /// Turns of `profile` running now.
    pub(crate) fn active(&self, profile: &str) -> usize {
        self.running
            .lock()
            .expect("limits poisoned")
            .get(profile)
            .map_or(0, |c| Arc::strong_count(c) - 1)
    }
}

#[cfg(test)]
mod tests {
    use super::Limits;

    /// Scenario: Eleventh run refused.
    #[test]
    fn the_run_past_the_cap_is_refused_until_one_exits() {
        let limits = Limits::default();
        let permits: Vec<_> = (0..10).map(|_| limits.acquire("p", 10).unwrap()).collect();
        let refused = limits.acquire("p", 10).unwrap_err();
        assert_eq!(refused.status(), 429);
        assert_eq!(refused.headers()["retry-after"], "5");
        assert!(limits.acquire("other", 10).is_ok());
        drop(permits);
        assert_eq!(limits.active("p"), 0);
        assert!(limits.acquire("p", 10).is_ok());
        assert!(limits.acquire("p", 0).is_ok());
    }
}

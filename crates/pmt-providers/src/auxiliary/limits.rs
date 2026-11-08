//! Auxiliary concurrency (spec: model-providers, Auxiliary concurrency and
//! request overrides): at most `auxiliary.max_concurrency` auxiliary calls
//! of a profile are in flight at once across all its sessions, and at most
//! `[auxiliary.<task>] max_concurrency` of one task; further calls wait
//! their turn.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::AuxTask;

/// The concurrency caps of one profile's auxiliary calls.
#[derive(Debug)]
pub struct AuxLimits {
    max: u32,
    total: Arc<Semaphore>,
    tasks: Mutex<HashMap<AuxTask, (u32, Arc<Semaphore>)>>,
}

/// Held while a call is in flight.
#[derive(Debug)]
pub struct Slot {
    _task: Option<OwnedSemaphorePermit>,
    _total: OwnedSemaphorePermit,
}

impl AuxLimits {
    /// At most `max` calls in flight (at least one).
    #[must_use]
    pub fn new(max: u32) -> Self {
        let max = max.max(1);
        Self {
            max,
            total: Arc::new(Semaphore::new(max as usize)),
            tasks: Mutex::new(HashMap::new()),
        }
    }

    /// The caps of the profile at `dir`, shared by every session of this
    /// process; a changed `max` starts new caps (calls already holding a
    /// slot finish under the old ones).
    #[must_use]
    pub fn for_profile(dir: &Path, max: u32) -> Arc<Self> {
        static PROFILES: OnceLock<Mutex<HashMap<PathBuf, Arc<AuxLimits>>>> = OnceLock::new();
        let mut profiles = PROFILES
            .get_or_init(Mutex::default)
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let limits = profiles
            .entry(dir.to_path_buf())
            .or_insert_with(|| Arc::new(Self::new(max)));
        if limits.max != max.max(1) {
            *limits = Arc::new(Self::new(max));
        }
        Arc::clone(limits)
    }

    /// Waits for a slot of `task` (capped at `task_max` when set), then a
    /// profile slot: a call waiting on its task's cap holds no profile
    /// slot.
    ///
    /// # Panics
    ///
    /// Never: the semaphores are never closed.
    pub async fn acquire(&self, task: AuxTask, task_max: Option<u32>) -> Slot {
        let task_permit = match task_max {
            Some(max) => {
                let semaphore = self.task_semaphore(task, max.max(1));
                semaphore.acquire_owned().await.ok()
            }
            None => None,
        };
        let total = Arc::clone(&self.total)
            .acquire_owned()
            .await
            .expect("the semaphore is never closed");
        Slot {
            _task: task_permit,
            _total: total,
        }
    }

    fn task_semaphore(&self, task: AuxTask, max: u32) -> Arc<Semaphore> {
        let mut tasks = self.tasks.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = tasks
            .entry(task)
            .or_insert_with(|| (max, Arc::new(Semaphore::new(max as usize))));
        if entry.0 != max {
            *entry = (max, Arc::new(Semaphore::new(max as usize)));
        }
        Arc::clone(&entry.1)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;

    async fn peak(limits: Arc<AuxLimits>, task_max: Option<u32>, calls: usize) -> usize {
        let now = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let handles: Vec<_> = (0..calls)
            .map(|_| {
                let (limits, now, peak) =
                    (Arc::clone(&limits), Arc::clone(&now), Arc::clone(&peak));
                tokio::spawn(async move {
                    let _slot = limits.acquire(AuxTask::Compression, task_max).await;
                    let inside = now.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(inside, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    now.fetch_sub(1, Ordering::SeqCst);
                })
            })
            .collect();
        for handle in handles {
            handle.await.unwrap();
        }
        peak.load(Ordering::SeqCst)
    }

    #[tokio::test(start_paused = true)]
    async fn the_task_cap_and_the_profile_cap_both_hold() {
        assert_eq!(peak(Arc::new(AuxLimits::new(8)), Some(2), 5).await, 2);
        assert_eq!(peak(Arc::new(AuxLimits::new(3)), None, 5).await, 3);
        assert_eq!(peak(Arc::new(AuxLimits::new(0)), None, 3).await, 1);
    }

    #[test]
    fn sessions_of_one_profile_share_its_caps() {
        let dir = Path::new("/profiles/limits-test");
        let a = AuxLimits::for_profile(dir, 4);
        let b = AuxLimits::for_profile(dir, 4);
        assert!(Arc::ptr_eq(&a, &b));
        let changed = AuxLimits::for_profile(dir, 6);
        assert!(!Arc::ptr_eq(&a, &changed));
    }
}

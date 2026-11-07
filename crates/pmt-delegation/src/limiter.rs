//! The profile's child concurrency limit (spec: subagent-delegation,
//! Concurrency limit and fair queueing).
//!
//! At most `max_concurrent_children` children run at once per profile,
//! synchronous and background alike. A child beyond that waits in a queue:
//! FIFO within its root session, round-robin across root sessions, so one
//! busy conversation cannot starve another. Once `max_queued` children
//! wait, further spawns are refused with `delegation.at_capacity`.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};

use pmt_core::error::codes;
use tokio::sync::oneshot;

use crate::Refusal;

/// One profile's limiter.
#[derive(Debug)]
pub struct Limiter {
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    max: usize,
    max_queued: usize,
    running: usize,
    queued: usize,
    /// Waiting children by root session, in round-robin order.
    roots: VecDeque<(String, VecDeque<oneshot::Sender<Permit>>)>,
}

/// A running slot; dropping it admits the next waiting child.
#[derive(Debug)]
pub struct Permit {
    limiter: Arc<Limiter>,
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.limiter.release();
    }
}

/// A child's place: running at once, or queued.
#[derive(Debug)]
pub enum Admission {
    /// It may run now.
    Running(Permit),
    /// It waits for a slot.
    Queued(oneshot::Receiver<Permit>),
}

impl Admission {
    /// Whether the child waits.
    #[must_use]
    pub fn is_queued(&self) -> bool {
        matches!(self, Self::Queued(_))
    }

    /// Waits for the slot (`None` when the limiter went away).
    pub async fn ready(self) -> Option<Permit> {
        match self {
            Self::Running(permit) => Some(permit),
            Self::Queued(waiting) => waiting.await.ok(),
        }
    }
}

static PROFILES: LazyLock<Mutex<HashMap<PathBuf, Arc<Limiter>>>> = LazyLock::new(Mutex::default);

impl Limiter {
    /// A limiter of `max` running and `max_queued` waiting children.
    #[must_use]
    pub fn new(max: u32, max_queued: u32) -> Arc<Self> {
        let limiter = Arc::new(Self {
            state: Mutex::default(),
        });
        limiter.set_limits(max, max_queued);
        limiter
    }

    /// The process-wide limiter of the profile at `dir`, with its limits
    /// set to `max` and `max_queued` (the latest configuration wins).
    #[must_use]
    pub fn for_profile(dir: &Path, max: u32, max_queued: u32) -> Arc<Self> {
        let limiter = Arc::clone(
            PROFILES
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(dir.to_path_buf())
                .or_insert_with(|| Self::new(max, max_queued)),
        );
        limiter.set_limits(max, max_queued);
        limiter
    }

    fn set_limits(&self, max: u32, max_queued: u32) {
        let mut state = self.lock();
        state.max = usize::try_from(max.max(1)).unwrap_or(1);
        state.max_queued = usize::try_from(max_queued).unwrap_or(0);
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Children running now.
    #[must_use]
    pub fn running(&self) -> usize {
        self.lock().running
    }

    /// Admits `n` children of root session `root` at once: as many as
    /// there are free slots run, the rest queue.
    ///
    /// # Errors
    ///
    /// `delegation.at_capacity` when the queue cannot take the rest
    /// (nothing is admitted).
    pub fn admit(self: &Arc<Self>, root: &str, n: usize) -> Result<Vec<Admission>, Refusal> {
        let mut state = self.lock();
        let free = state.max.saturating_sub(state.running);
        let waiting = n.saturating_sub(free);
        if state.queued + waiting > state.max_queued {
            return Err(Refusal::new(
                &codes::DELEGATION_AT_CAPACITY,
                format!(
                    "Delegation is at capacity: {} children run and {} wait \
                     (delegation.max_concurrent_children = {}, delegation.max_queued = {}).",
                    state.running, state.queued, state.max, state.max_queued
                ),
            ));
        }
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            if i < free {
                state.running += 1;
                out.push(Admission::Running(Permit {
                    limiter: Arc::clone(self),
                }));
            } else {
                let (tx, rx) = oneshot::channel();
                state.queued += 1;
                match state.roots.iter_mut().find(|(r, _)| r == root) {
                    Some((_, queue)) => queue.push_back(tx),
                    None => state
                        .roots
                        .push_back((root.to_owned(), VecDeque::from([tx]))),
                }
                out.push(Admission::Queued(rx));
            }
        }
        Ok(out)
    }

    /// Frees a slot and hands it to the next waiting child: the front of
    /// the next root session's queue, that root moving to the back.
    fn release(self: &Arc<Self>) {
        let mut undelivered = Vec::new();
        {
            let mut state = self.lock();
            state.running = state.running.saturating_sub(1);
            while state.running < state.max {
                let Some((root, mut queue)) = state.roots.pop_front() else {
                    break;
                };
                let next = queue.pop_front();
                if !queue.is_empty() {
                    state.roots.push_back((root, queue));
                }
                let Some(tx) = next else { continue };
                state.queued = state.queued.saturating_sub(1);
                state.running += 1;
                let permit = Permit {
                    limiter: Arc::clone(self),
                };
                if let Err(permit) = tx.send(permit) {
                    // The waiter gave up; its permit is released below,
                    // outside the lock, which admits the next one.
                    undelivered.push(permit);
                }
            }
        }
        drop(undelivered);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scenario: Excess children queue rather than oversubscribe.
    #[tokio::test]
    async fn a_third_child_queues_and_starts_when_one_finishes() {
        let limiter = Limiter::new(2, 32);
        let mut admitted = limiter.admit("root", 3).unwrap();
        assert!(!admitted[0].is_queued() && !admitted[1].is_queued());
        assert!(admitted[2].is_queued());
        assert_eq!(limiter.running(), 2);
        let third = admitted.pop().unwrap();
        let first = admitted.remove(0).ready().await.unwrap();
        drop(first);
        let _third = third.ready().await.unwrap();
        assert_eq!(limiter.running(), 2);
    }

    #[tokio::test]
    async fn waiting_roots_are_served_round_robin() {
        let limiter = Limiter::new(1, 32);
        let running = limiter
            .admit("a", 1)
            .unwrap()
            .pop()
            .unwrap()
            .ready()
            .await
            .unwrap();
        let mut a = limiter.admit("a", 2).unwrap();
        let mut b = limiter.admit("b", 1).unwrap();
        drop(running);
        let a1 = a.remove(0).ready().await.unwrap();
        drop(a1);
        // b's child goes before a's second one.
        let b1 = b.remove(0).ready().await.unwrap();
        assert!(matches!(a[0], Admission::Queued(_)));
        drop(b1);
        a.remove(0).ready().await.unwrap();
    }

    #[test]
    fn a_full_queue_refuses_the_whole_call() {
        let limiter = Limiter::new(1, 1);
        let _held = limiter.admit("a", 2).unwrap();
        let err = limiter.admit("b", 1).unwrap_err();
        assert_eq!(err.code.code, "delegation.at_capacity");
    }
}

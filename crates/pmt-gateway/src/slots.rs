//! The daemon's turn slots (spec: messaging-gateway, Concurrency caps
//! across surfaces): `gateway.limits.max_concurrent_sessions` in
//! `daemon.toml` bounds how many sessions run a turn at once across every
//! profile and surface (`0`: unbounded). A turn beyond the cap waits in
//! one first-in-first-out line instead of being refused, and is told how
//! many turns are ahead of it.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use tokio::sync::oneshot;

/// What a turn waiting for a slot is told.
#[must_use]
pub fn waiting_notice(ahead: usize) -> String {
    format!("Waiting for a free slot ({ahead} ahead)")
}

/// The slots.
#[derive(Debug, Clone)]
pub struct Slots(Arc<Mutex<State>>);

#[derive(Debug)]
struct State {
    max: usize,
    running: usize,
    waiting: VecDeque<oneshot::Sender<()>>,
}

/// A running turn's slot, freed (and handed to the next waiting turn)
/// when dropped.
#[derive(Debug)]
pub struct Permit {
    slots: Option<Slots>,
}

impl Drop for Permit {
    fn drop(&mut self) {
        let Some(slots) = self.slots.take() else {
            return;
        };
        let mut state = slots.0.lock().expect("slots poisoned");
        // The slot passes straight to the first waiter still waiting.
        while let Some(next) = state.waiting.pop_front() {
            if next.send(()).is_ok() {
                return;
            }
        }
        state.running = state.running.saturating_sub(1);
    }
}

impl Slots {
    /// Slots for `max` concurrent turns (`0`: unbounded).
    #[must_use]
    pub fn new(max: u32) -> Self {
        Self(Arc::new(Mutex::new(State {
            max: max as usize,
            running: 0,
            waiting: VecDeque::new(),
        })))
    }

    /// A slot: at once while one is free, otherwise after the turns ahead
    /// (`waiting` is told how many there are before this one waits).
    ///
    /// # Panics
    ///
    /// When another holder panicked while holding the slots' lock.
    pub async fn acquire(&self, waiting: impl FnOnce(usize)) -> Permit {
        let rx = {
            let mut state = self.0.lock().expect("slots poisoned");
            if state.max == 0 {
                return Permit { slots: None };
            }
            if state.running < state.max && state.waiting.is_empty() {
                state.running += 1;
                return Permit {
                    slots: Some(self.clone()),
                };
            }
            let ahead = state.waiting.len() + 1;
            let (tx, rx) = oneshot::channel();
            state.waiting.push_back(tx);
            drop(state);
            waiting(ahead);
            rx
        };
        // The sender lives in the line until a permit hands it the slot.
        let _ = rx.await;
        Permit {
            slots: Some(self.clone()),
        }
    }

    /// Turns running with a slot, and turns waiting for one.
    ///
    /// # Panics
    ///
    /// When another holder panicked while holding the slots' lock.
    #[must_use]
    pub fn load(&self) -> (usize, usize) {
        let state = self.0.lock().expect("slots poisoned");
        (state.running, state.waiting.len())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn a_turn_beyond_the_cap_waits_its_turn_in_order() {
        let slots = Slots::new(2);
        let first = slots.acquire(|_| panic!("free")).await;
        let _second = slots.acquire(|_| panic!("free")).await;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        for n in 0..2 {
            let slots = slots.clone();
            let tx = tx.clone();
            pmt_core::spawn::spawn(async move {
                let told = tx.clone();
                let permit = slots
                    .acquire(move |ahead| {
                        let _ = told.send(format!("{n} waits ({ahead} ahead)"));
                    })
                    .await;
                let _ = tx.send(format!("{n} runs"));
                tokio::time::sleep(Duration::from_millis(20)).await;
                drop(permit);
            });
            assert_eq!(
                rx.recv().await.unwrap(),
                format!("{n} waits ({} ahead)", n + 1)
            );
        }
        assert_eq!(slots.load(), (2, 2));
        drop(first);
        assert_eq!(rx.recv().await.unwrap(), "0 runs");
        assert_eq!(rx.recv().await.unwrap(), "1 runs", "first in, first out");
    }

    #[tokio::test]
    async fn zero_is_unbounded() {
        let slots = Slots::new(0);
        let _held: Vec<Permit> = futures_util::future::join_all(
            (0..50).map(|_| slots.acquire(|_| panic!("never waits"))),
        )
        .await;
    }
}

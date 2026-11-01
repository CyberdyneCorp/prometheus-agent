//! One turn at a time per session (spec: agent-turn-loop, Single active
//! turn per session): a durable turn lease in `state.db`, waited for at
//! admission and renewed while the turn runs. Losing it mid-turn
//! hard-interrupts the turn with system issuer `lease_lost`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use pmt_core::error::{Error, codes};
use pmt_core::id::SessionId;
use pmt_store::{Acquire, Holder, Lease, LeaseKind, Store};
use tokio::task::JoinHandle;

use super::interrupt::{Interrupt, Issuer};

/// Lease lifetimes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeaseTiming {
    /// Lifetime of the lease unless renewed.
    pub ttl: Duration,
    /// How often the running turn renews it.
    pub renew_every: Duration,
    /// How long admission waits for another holder to finish.
    pub admission_wait: Duration,
}

impl Default for LeaseTiming {
    fn default() -> Self {
        Self {
            ttl: Duration::from_mins(5),
            renew_every: Duration::from_mins(1),
            admission_wait: Duration::from_mins(30),
        }
    }
}

/// How often admission retries a busy lease.
const ADMISSION_POLL: Duration = Duration::from_millis(250);

/// Waits up to `timing.admission_wait` for the turn lease of `session`.
///
/// # Errors
///
/// `turn.lease_unavailable` when another holder kept it; store errors.
pub async fn admit(store: &Store, session: SessionId, timing: LeaseTiming) -> Result<Lease, Error> {
    let started = Instant::now();
    let holder = Holder::current();
    loop {
        match store.acquire_lease_for(session, LeaseKind::Turn, &holder, timing.ttl)? {
            Acquire::Granted(lease) => return Ok(lease),
            Acquire::Busy { holder, .. } => {
                let waited = started.elapsed();
                if waited >= timing.admission_wait {
                    return Err(Error::new(
                        &codes::TURN_LEASE_UNAVAILABLE,
                        format!(
                            "session {session} is running a turn in process {} on host {}",
                            holder.pid, holder.host_id
                        ),
                    ));
                }
                tokio::time::sleep(
                    ADMISSION_POLL.min(timing.admission_wait.saturating_sub(waited)),
                )
                .await;
            }
        }
    }
}

/// Renews the lease every `renew_every` until dropped; a failed renewal
/// interrupts the turn with issuer `lease_lost`.
pub struct Refresher {
    task: JoinHandle<()>,
}

impl Refresher {
    /// Starts renewing `lease`.
    #[must_use]
    pub fn start(
        store: Arc<Store>,
        mut lease: Lease,
        every: Duration,
        interrupt: Interrupt,
    ) -> Self {
        let task = tokio::spawn(async move {
            loop {
                tokio::time::sleep(every).await;
                if let Err(err) = store.renew_lease(&mut lease) {
                    tracing::warn!(error = %err, "turn lease lost; interrupting the turn");
                    interrupt.stop(Issuer::System("lease_lost".to_owned()));
                    return;
                }
            }
        });
        Self { task }
    }
}

impl Drop for Refresher {
    fn drop(&mut self) {
        self.task.abort();
    }
}

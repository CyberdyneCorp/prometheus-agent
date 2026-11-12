//! Ctrl+C during a migration (spec: hermes-migration, Per-component
//! atomicity and resumability): the signal only raises a flag, and the
//! migrator stops at its next commit boundary (between components and
//! between session batches), so what was committed is complete and in the
//! ledger and a re-run continues from there.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::chat::signals::Signals;

/// The interrupt flag and the runtime its signal listener lives on.
pub(super) struct Interrupt {
    raised: Arc<AtomicBool>,
    // Kept alive for the whole run: dropping it removes the listener and
    // brings back the default action (killing the process mid-commit).
    _runtime: Option<tokio::runtime::Runtime>,
}

impl Interrupt {
    /// Installs the SIGINT/SIGTERM (Ctrl+C, Ctrl+Break) listener; without
    /// a runtime the run cannot be interrupted gracefully.
    pub(super) fn listen() -> Self {
        let raised = Arc::new(AtomicBool::new(false));
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .ok();
        if let Some(rt) = &runtime {
            let flag = Arc::clone(&raised);
            let _guard = rt.enter();
            let mut signals = Signals::listen();
            pmt_core::spawn::spawn_on(rt.handle(), async move {
                signals.next().await;
                flag.store(true, Ordering::SeqCst);
            });
        }
        Self {
            raised,
            _runtime: runtime,
        }
    }

    /// A flag no signal raises (planning before the confirmation, where
    /// Ctrl+C keeps its default action).
    pub(super) fn none() -> Self {
        Self {
            raised: Arc::new(AtomicBool::new(false)),
            _runtime: None,
        }
    }

    /// Whether a signal arrived.
    pub(super) fn raised(&self) -> bool {
        self.raised.load(Ordering::SeqCst)
    }
}

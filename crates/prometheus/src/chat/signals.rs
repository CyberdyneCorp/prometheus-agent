//! SIGINT and SIGTERM while a non-interactive run works: the turn is
//! stopped (its partial transcript kept) and the process exits 130 or 143
//! (spec: cli, Stable exit codes).
//!
//! The listeners are installed once, before the turn starts, and live as
//! long as the run: a listener made per wait would leave the default
//! action (killing the process mid-turn) before the first wait and lose a
//! signal that arrives between two waits.

/// The run's signal listeners.
pub(super) struct Signals {
    #[cfg(unix)]
    listeners: Option<(tokio::signal::unix::Signal, tokio::signal::unix::Signal)>,
    #[cfg(windows)]
    listener: Option<tokio::signal::windows::CtrlC>,
}

impl Signals {
    /// Installs the listeners (inside the Tokio runtime); a run whose
    /// listeners cannot be installed is never stopped by a signal.
    #[cfg(unix)]
    pub(super) fn listen() -> Self {
        use tokio::signal::unix::{SignalKind, signal};
        let listeners = signal(SignalKind::interrupt())
            .and_then(|int| Ok((int, signal(SignalKind::terminate())?)))
            .ok();
        Self { listeners }
    }

    /// Installs the Ctrl-C listener (inside the Tokio runtime).
    #[cfg(windows)]
    pub(super) fn listen() -> Self {
        Self {
            listener: tokio::signal::windows::ctrl_c().ok(),
        }
    }

    /// The exit status of the next SIGINT (130) or SIGTERM (143).
    #[cfg(unix)]
    pub(super) async fn next(&mut self) -> u8 {
        let Some((int, term)) = self.listeners.as_mut() else {
            return std::future::pending().await;
        };
        tokio::select! {
            _ = int.recv() => pmt_core::exit::INTERRUPTED,
            _ = term.recv() => pmt_core::exit::TERMINATED,
        }
    }

    /// The exit status of the next Ctrl-C (130).
    #[cfg(windows)]
    pub(super) async fn next(&mut self) -> u8 {
        let Some(listener) = self.listener.as_mut() else {
            return std::future::pending().await;
        };
        if listener.recv().await.is_none() {
            return std::future::pending().await;
        }
        pmt_core::exit::INTERRUPTED
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::time::Duration;

    use nix::sys::signal::{Signal, raise};

    use super::Signals;

    #[tokio::test]
    async fn a_signal_before_the_first_wait_is_handled() {
        let mut signals = Signals::listen();
        raise(Signal::SIGINT).unwrap();
        assert_eq!(signals.next().await, pmt_core::exit::INTERRUPTED);
    }

    #[tokio::test]
    async fn a_signal_between_two_waits_is_not_lost() {
        let mut signals = Signals::listen();
        let waited = tokio::time::timeout(Duration::from_millis(10), signals.next()).await;
        assert!(waited.is_err(), "no signal was sent yet");
        raise(Signal::SIGTERM).unwrap();
        assert_eq!(signals.next().await, pmt_core::exit::TERMINATED);
    }
}

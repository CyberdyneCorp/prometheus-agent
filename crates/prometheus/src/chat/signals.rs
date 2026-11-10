//! SIGINT and SIGTERM while a non-interactive run works: the turn is
//! stopped (its partial transcript kept) and the process exits 130 or 143
//! (spec: cli, Stable exit codes). On Windows, Ctrl+C and Ctrl+Break are
//! the interrupt (130): Ctrl+Break is the one console event a parent can
//! send to a single child (the smoke suite's interrupt task does).
//!
//! The listeners are installed once, before the turn starts, and live as
//! long as the run: a listener made per wait would leave the default
//! action (killing the process mid-turn) before the first wait and lose a
//! signal that arrives between two waits.

/// The run's signal listeners.
pub(crate) struct Signals {
    #[cfg(unix)]
    listeners: Option<(tokio::signal::unix::Signal, tokio::signal::unix::Signal)>,
    #[cfg(windows)]
    listeners: Option<(
        tokio::signal::windows::CtrlC,
        tokio::signal::windows::CtrlBreak,
    )>,
}

impl Signals {
    /// Installs the listeners (inside the Tokio runtime); a run whose
    /// listeners cannot be installed is never stopped by a signal.
    #[cfg(unix)]
    pub(crate) fn listen() -> Self {
        use tokio::signal::unix::{SignalKind, signal};
        let listeners = signal(SignalKind::interrupt())
            .and_then(|int| Ok((int, signal(SignalKind::terminate())?)))
            .ok();
        Self { listeners }
    }

    /// Installs the Ctrl+C and Ctrl+Break listeners (inside the Tokio
    /// runtime).
    #[cfg(windows)]
    pub(crate) fn listen() -> Self {
        use tokio::signal::windows::{ctrl_break, ctrl_c};
        let listeners = ctrl_c().and_then(|c| Ok((c, ctrl_break()?))).ok();
        Self { listeners }
    }

    /// The exit status of the next SIGINT (130) or SIGTERM (143).
    #[cfg(unix)]
    pub(crate) async fn next(&mut self) -> u8 {
        let Some((int, term)) = self.listeners.as_mut() else {
            return std::future::pending().await;
        };
        tokio::select! {
            _ = int.recv() => pmt_core::exit::INTERRUPTED,
            _ = term.recv() => pmt_core::exit::TERMINATED,
        }
    }

    /// The exit status of the next Ctrl+C or Ctrl+Break (130).
    #[cfg(windows)]
    pub(crate) async fn next(&mut self) -> u8 {
        let Some((c, brk)) = self.listeners.as_mut() else {
            return std::future::pending().await;
        };
        let received = tokio::select! {
            got = c.recv() => got,
            got = brk.recv() => got,
        };
        if received.is_none() {
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

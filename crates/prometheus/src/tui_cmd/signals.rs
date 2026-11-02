//! Signals that end the TUI (spec: tui, Terminal state is always
//! restored): SIGINT exits 130, SIGTERM and SIGHUP (the terminal went
//! away) exit 143, each after the terminal is restored. In raw mode Ctrl+C
//! is a key, not SIGINT: only a signal sent from outside arrives here.

/// The TUI's signal listeners, installed before the screen opens.
pub(super) struct Signals {
    #[cfg(unix)]
    listeners: Option<[tokio::signal::unix::Signal; 3]>,
    #[cfg(windows)]
    listener: Option<tokio::signal::windows::CtrlC>,
}

impl Signals {
    /// Installs the listeners (inside the runtime).
    #[cfg(unix)]
    pub(super) fn listen() -> Self {
        use tokio::signal::unix::{SignalKind, signal};
        let listeners = signal(SignalKind::interrupt())
            .and_then(|int| {
                Ok([
                    int,
                    signal(SignalKind::terminate())?,
                    signal(SignalKind::hangup())?,
                ])
            })
            .ok();
        Self { listeners }
    }

    /// Installs the Ctrl-C listener (inside the runtime).
    #[cfg(windows)]
    pub(super) fn listen() -> Self {
        Self {
            listener: tokio::signal::windows::ctrl_c().ok(),
        }
    }

    /// The exit status of the next signal.
    #[cfg(unix)]
    pub(super) async fn next(&mut self) -> u8 {
        let Some([int, term, hup]) = self.listeners.as_mut() else {
            return std::future::pending().await;
        };
        tokio::select! {
            _ = int.recv() => pmt_core::exit::INTERRUPTED,
            _ = term.recv() => pmt_core::exit::TERMINATED,
            _ = hup.recv() => pmt_core::exit::TERMINATED,
        }
    }

    /// The exit status of the next Ctrl-C.
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

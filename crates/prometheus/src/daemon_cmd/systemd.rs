//! systemd service notifications (`sd_notify`): `READY=1` once the daemon
//! serves and `WATCHDOG=1` heartbeats from the liveness watchdog, sent to
//! `$NOTIFY_SOCKET` only when `daemon.systemd_watchdog_secs > 0` (Linux).

/// Sends notifications to the service manager.
#[derive(Debug, Clone)]
pub(super) struct Notifier {
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    socket: std::ffi::OsString,
}

impl Notifier {
    /// The notifier of a daemon started by systemd with a watchdog; `None`
    /// elsewhere.
    pub(super) fn from(launch: &pmt_core::env::LaunchEnv, watchdog_secs: u64) -> Option<Self> {
        if watchdog_secs == 0 || !cfg!(target_os = "linux") {
            return None;
        }
        launch
            .inherited("NOTIFY_SOCKET")
            .filter(|s| !s.is_empty())
            .map(|socket| Self {
                socket: socket.to_owned(),
            })
    }

    /// Sends `message` (`READY=1`, `WATCHDOG=1`); a failure is logged at
    /// DEBUG.
    #[cfg_attr(not(target_os = "linux"), allow(clippy::unused_self))]
    pub(super) fn notify(&self, message: &str) {
        #[cfg(target_os = "linux")]
        if let Err(err) = self.send(message) {
            tracing::debug!("sd_notify {message}: {err}");
        }
        #[cfg(not(target_os = "linux"))]
        let _ = message;
    }

    #[cfg(target_os = "linux")]
    fn send(&self, message: &str) -> std::io::Result<()> {
        use std::os::linux::net::SocketAddrExt as _;
        use std::os::unix::ffi::OsStrExt as _;
        use std::os::unix::net::{SocketAddr, UnixDatagram};
        let bytes = self.socket.as_bytes();
        let addr = match bytes.strip_prefix(b"@") {
            Some(abstract_name) => SocketAddr::from_abstract_name(abstract_name)?,
            None => SocketAddr::from_pathname(std::path::Path::new(&self.socket))?,
        };
        UnixDatagram::unbound()?.send_to_addr(message.as_bytes(), &addr)?;
        Ok(())
    }
}

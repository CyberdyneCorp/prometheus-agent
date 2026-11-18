//! What starting a bot screen leaves, wherever it runs: the processes the
//! supervisor watches and how this host's clients reach the screen.

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use pmt_core::error::{Error, codes};

/// How long Xvnc may take to create its RFB socket.
pub(super) const XVNC_START: Duration = Duration::from_secs(15);

/// A started screen. Dropping it ends its processes (each in its own
/// process group) and then removes what lives in the backend.
pub(super) struct Launched {
    /// The processes the screen lives as long as, by name (the stop
    /// reason names the one that exited).
    pub(super) children: Vec<(&'static str, Child)>,
    /// The RFB socket on this host.
    pub(super) rfb: PathBuf,
    /// The X display this host's X clients (headed browsers) use.
    pub(super) display: String,
    /// The display inside the backend (`None` on the gateway host).
    pub(super) backend_display: Option<String>,
    /// `host`, `container` or `ssh`.
    pub(super) placement: &'static str,
    /// Removes the screen's remains in the backend.
    pub(super) cleanup: Option<Box<dyn FnOnce()>>,
}

impl Launched {
    /// A screen of `placement` whose RFB socket will be `rfb`.
    pub(super) fn new(placement: &'static str, rfb: PathBuf) -> Self {
        Self {
            children: Vec::new(),
            rfb,
            display: String::new(),
            backend_display: None,
            placement,
            cleanup: None,
        }
    }

    /// The first watched process that exited.
    pub(super) fn exited(&mut self) -> Option<&'static str> {
        self.children
            .iter_mut()
            .find_map(|(name, child)| (!matches!(child.try_wait(), Ok(None))).then_some(*name))
    }
}

impl Drop for Launched {
    fn drop(&mut self) {
        for (_, child) in self.children.iter_mut().rev() {
            end(child);
        }
        if let Some(cleanup) = self.cleanup.take() {
            cleanup();
        }
    }
}

/// Ends `child` and every helper of its process group.
pub(super) fn end(child: &mut Child) {
    pmt_tools::browser::cleanup::kill_group(child.id());
    let _ = child.kill();
    let _ = child.wait();
}

/// Spawns `command` in a process group of its own.
///
/// # Errors
///
/// `screen.unavailable` naming `what`.
pub(super) fn spawn(mut command: Command, what: &str) -> Result<Child, Error> {
    pmt_tools::browser::cleanup::new_group(&mut command);
    command.spawn().map_err(|e| failed(&format!("{what}: {e}")))
}

/// Waits for `socket` to appear while `child` (what creates it, or
/// relays it) runs.
///
/// # Errors
///
/// Why it did not: the child exited (its last stderr line) or the wait
/// ran out.
pub(super) fn wait_for_socket(child: &mut Child, socket: &Path, what: &str) -> Result<(), String> {
    use std::io::Read as _;
    let deadline = Instant::now() + XVNC_START;
    while Instant::now() < deadline {
        if socket.exists() {
            return Ok(());
        }
        if let Ok(Some(status)) = child.try_wait() {
            let mut why = String::new();
            if let Some(mut stderr) = child.stderr.take() {
                let _ = stderr.read_to_string(&mut why);
            }
            return Err(format!("{what} exited ({status}): {}", last_line(&why)));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(format!(
        "{what} did not create {} within {} s",
        socket.display(),
        XVNC_START.as_secs()
    ))
}

/// Discards what `child` still writes to its piped stderr, once its
/// start-up errors are no longer needed: a full pipe would block it (Xvnc
/// logs every viewer connection).
pub(super) fn drain_stderr(child: &mut Child) {
    if let Some(mut stderr) = child.stderr.take() {
        std::thread::spawn(move || std::io::copy(&mut stderr, &mut std::io::sink()));
    }
}

/// The last non-empty line of `text`.
pub(super) fn last_line(text: &str) -> &str {
    text.lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
}

/// `screen.unavailable`: the screen cannot start.
pub(super) fn failed(why: &str) -> Error {
    Error::new(
        &codes::SCREEN_UNAVAILABLE,
        format!("the bot screen cannot start: {why}"),
    )
}

/// The Xvnc arguments after the display: RFB only on the Unix socket
/// `rfb` with `mode`, never on TCP, shared between viewers.
pub(super) fn xvnc_args(geometry: &str, rfb: &str, mode: &str) -> Vec<String> {
    [
        "-geometry",
        geometry,
        "-depth",
        "24",
        "-rfbunixpath",
        rfb,
        "-rfbunixmode",
        mode,
        "-rfbport",
        "-1",
        "-SecurityTypes",
        "None",
        "-nolisten",
        "tcp",
        "-AlwaysShared",
    ]
    .map(str::to_owned)
    .to_vec()
}

/// The desktop session started on `$DISPLAY`: Xfce under its own D-Bus
/// session bus when `dbus-launch` is installed.
pub(super) const SESSION_SCRIPT: &str = "if command -v dbus-launch >/dev/null 2>&1; then exec dbus-launch --exit-with-session xfce4-session; else exec xfce4-session; fi";

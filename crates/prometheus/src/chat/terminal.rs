//! The identity of the controlling terminal (spec: cli, Resume and
//! continue resolution: "the terminal is identified by its controlling TTY
//! device and session leader, never by argv"), and whether stdin and
//! stdout are terminals.

use std::io::IsTerminal as _;

/// Both stdin and stdout are terminals: the only case that is
/// interactive (spec: cli, Interface selection is TTY-first).
#[must_use]
pub fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// `<tty device>#<session leader pid>` of this process, when it has a
/// controlling terminal on stdin.
#[cfg(unix)]
#[must_use]
pub fn identity() -> Option<String> {
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        return None;
    }
    let device = nix::unistd::ttyname(&stdin).ok()?;
    let leader = nix::unistd::getsid(None).ok()?;
    Some(format!("{}#{leader}", device.display()))
}

/// Windows consoles have no TTY device to name; `-c` then picks the
/// newest session of the directory.
#[cfg(not(unix))]
#[must_use]
pub fn identity() -> Option<String> {
    None
}

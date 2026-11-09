//! Passing descriptors to a confined child at fixed numbers (Chromium's
//! `--remote-debugging-pipe` reads fd 3 and writes fd 4). The only
//! `unsafe` here (health.toml allowlist): a `pre_exec` hook that runs
//! between `fork` and `exec` and issues only `fcntl(2)` and `dup2(2)`.

#![allow(unsafe_code)]

use std::io;
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd};
use std::os::unix::process::CommandExt as _;
use std::process::Command;

/// Lowest descriptor number the sources are moved to in the parent, so a
/// source never sits on a target number.
const SOURCE_FLOOR: RawFd = 10;

/// Makes the child of `command` get each `(source, target)` descriptor as
/// `target` (inherited across `exec`). Call it after the sandbox built the
/// command: its hook runs last. A descriptor the earlier hooks left
/// inheritable on a target number (a sandbox's liveness channel) is moved
/// out of the way first, so it stays open in the child.
///
/// # Errors
///
/// A source cannot be duplicated.
pub fn pass(command: &mut Command, fds: Vec<(OwnedFd, RawFd)>) -> io::Result<()> {
    let mut moved = Vec::with_capacity(fds.len());
    for (source, target) in fds {
        // SAFETY: fcntl(2) F_DUPFD_CLOEXEC on a descriptor we own; the
        // result is a fresh descriptor whose ownership we take.
        let raw = unsafe { libc::fcntl(source.as_raw_fd(), libc::F_DUPFD_CLOEXEC, SOURCE_FLOOR) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` was just returned by fcntl and is owned by nobody.
        moved.push((unsafe { OwnedFd::from_raw_fd(raw) }, target));
    }
    let hook = move || -> io::Result<()> {
        for (_, target) in &moved {
            keep_inherited(*target)?;
        }
        for (source, target) in &moved {
            // SAFETY: dup2(2) of a descriptor the closure owns onto a plain
            // number; dup2 clears close-on-exec on the target.
            if unsafe { libc::dup2(source.as_raw_fd(), *target) } < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    };
    // SAFETY: the hook runs between fork and exec and issues only fcntl(2)
    // and dup2(2) on descriptors and numbers fixed before the fork; it
    // allocates nothing, so it is async-signal-safe.
    unsafe {
        command.pre_exec(hook);
    }
    Ok(())
}

/// Moves an inheritable descriptor at `target` to a free number above
/// [`SOURCE_FLOOR`] (still inheritable), so `dup2` onto `target` does not
/// close it.
fn keep_inherited(target: RawFd) -> io::Result<()> {
    // SAFETY: fcntl(2) F_GETFD on a plain number; fails with EBADF when
    // nothing is open there.
    let flags = unsafe { libc::fcntl(target, libc::F_GETFD) };
    if flags < 0 || flags & libc::FD_CLOEXEC != 0 {
        return Ok(());
    }
    // SAFETY: fcntl(2) F_DUPFD on an open descriptor.
    if unsafe { libc::fcntl(target, libc::F_DUPFD, SOURCE_FLOOR) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};

    use super::*;

    #[test]
    fn the_child_reads_and_writes_the_passed_descriptors() {
        let (to_child_read, mut to_child_write) = std::io::pipe().unwrap();
        let (mut from_child_read, from_child_write) = std::io::pipe().unwrap();
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "read line <&3; printf 'got %s' \"$line\" >&4"]);
        pass(
            &mut command,
            vec![
                (OwnedFd::from(to_child_read), 3),
                (OwnedFd::from(from_child_write), 4),
            ],
        )
        .unwrap();
        let mut child = command.spawn().unwrap();
        drop(command);
        to_child_write.write_all(b"ping\n").unwrap();
        drop(to_child_write);
        assert!(child.wait().unwrap().success());
        let mut answer = String::new();
        from_child_read.read_to_string(&mut answer).unwrap();
        assert_eq!(answer, "got ping");
    }
}

//! A proxied child's own network namespace (spec: security-boundaries,
//! Egress enforcement for child processes; see [`crate::relay`]).
//!
//! Landlock's TCP rules name ports, not addresses: a child allowed to
//! connect to the egress proxy's port could connect to any host listening
//! on that port number. So a proxied child unshares a user and a network
//! namespace between `fork` and `exec`, brings its private loopback up,
//! binds `127.0.0.1:<proxy port>` there and sends the listening socket to
//! the parent over a socket pair; the parent's egress proxy accepts on it.
//! Nothing else leaves the namespace.
//!
//! The child keeps its end of the pair open across `exec` (and so do its
//! descendants): the parent's end reads end-of-file once all of them have
//! exited, which closes the listener.
//!
//! Unprivileged user namespaces can be disabled (sysctls, `AppArmor`, a
//! container's seccomp profile), so [`supported`] probes them once per
//! process; without them the `net` feature is `partial`.
//!
//! Like `exec.rs`, this module is on the `unsafe` allowlist: the hook runs
//! between fork and exec and issues only system calls on data prepared in
//! the parent (no allocation, no locks).

#![allow(unsafe_code)]

use std::io;
use std::net::TcpListener;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::sync::OnceLock;

use crate::relay::{Attached, Relay};

/// What the hook writes to `/proc/self/{uid,gid}_map`: the caller's own
/// ids map to themselves, so files keep their owner and programs that
/// check ownership (git) behave as outside.
struct Maps {
    uid: Vec<u8>,
    gid: Vec<u8>,
}

impl Maps {
    fn current() -> Self {
        // SAFETY: getuid(2) and getgid(2) cannot fail.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        Self {
            uid: format!("{uid} {uid} 1\n").into_bytes(),
            gid: format!("{gid} {gid} 1\n").into_bytes(),
        }
    }
}

/// The errno-shaped value the probe's hook reports when everything
/// worked (no system call returns it).
const PROBE_OK: i32 = 0x7e57_0001;

/// Whether this host lets an unprivileged process enter a user and
/// network namespace and bind a loopback listener there; probed once.
pub(super) fn supported() -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(probe)
}

/// Runs the namespace setup in a forked child that never executes
/// anything: the hook reports [`PROBE_OK`] (or the failing errno) as the
/// spawn error.
fn probe() -> bool {
    let maps = Maps::current();
    let mut command = Command::new("/");
    let hook = move || -> io::Result<()> {
        enter(&maps)?;
        let listener = listen(0)?;
        // SAFETY: close(2) of the descriptor listen() just returned.
        unsafe { libc::close(listener) };
        Err(io::Error::from_raw_os_error(PROBE_OK))
    };
    // SAFETY: the hook runs between fork and exec and issues only
    // unshare(2), open(2)/write(2)/close(2), socket(2), ioctl(2), bind(2)
    // and listen(2) on data built before the fork.
    unsafe {
        command.pre_exec(hook);
    }
    match command.spawn() {
        Err(err) => {
            let ok = err.raw_os_error() == Some(PROBE_OK);
            if !ok {
                tracing::debug!(error = %err, "no unprivileged network namespaces");
            }
            ok
        }
        Ok(mut child) => {
            let _ = child.kill();
            let _ = child.wait();
            false
        }
    }
}

/// Makes `command`'s child run in its own network namespace whose only
/// way out is a listener on `127.0.0.1:port`, handed to `relay`.
///
/// # Errors
///
/// The socket pair cannot be created, or the receiving thread cannot
/// start.
pub(super) fn isolate(command: &mut Command, port: u16, relay: &Relay) -> io::Result<()> {
    let (parent, child) = UnixStream::pair()?;
    let channel = child.as_raw_fd();
    let maps = Maps::current();
    let hook = move || -> io::Result<()> {
        // The closure owns `child`, so the descriptor stays open until the
        // command is dropped; the forked copy is the one used here.
        let _keep = &child;
        enter(&maps)?;
        let listener = listen(port)?;
        let sent = send_fd(channel, listener);
        // SAFETY: close(2) of our own copy; the parent holds the sent one.
        unsafe { libc::close(listener) };
        sent?;
        inherit(channel)
    };
    // SAFETY: as in `probe`, plus sendmsg(2) and fcntl(2) on the
    // inherited socket pair.
    unsafe {
        command.pre_exec(hook);
    }
    let relay = relay.clone();
    std::thread::Builder::new()
        .name("sandbox-netns".into())
        .spawn(move || receive(&parent, &relay))?;
    Ok(())
}

/// Hands every listener the command's children send to `relay` (one per
/// spawn), until all holders of the channel are gone.
fn receive(parent: &UnixStream, relay: &Relay) {
    loop {
        match recv_fd(parent).and_then(|got| Ok((got, parent.try_clone()?))) {
            Ok((Some(listener), alive)) => relay.serve(Attached { listener, alive }),
            Ok((None, _)) => return,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => {
                tracing::warn!(error = %err, "proxied child's listener not received");
                return;
            }
        }
    }
}

fn check(result: libc::c_int) -> io::Result<libc::c_int> {
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result)
    }
}

/// Enters a new user and network namespace, maps the caller's ids and
/// brings the namespace's loopback interface up.
fn enter(maps: &Maps) -> io::Result<()> {
    // SAFETY: unshare(2) with constant flags.
    check(unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNET) })?;
    write_file(c"/proc/self/setgroups", b"deny")?;
    write_file(c"/proc/self/uid_map", &maps.uid)?;
    write_file(c"/proc/self/gid_map", &maps.gid)?;
    loopback_up()
}

fn write_file(path: &std::ffi::CStr, contents: &[u8]) -> io::Result<()> {
    // SAFETY: open(2) of a NUL-terminated path.
    let fd = check(unsafe { libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC) })?;
    // SAFETY: write(2) of a live buffer to the descriptor just opened.
    let written = unsafe { libc::write(fd, contents.as_ptr().cast(), contents.len()) };
    let failed = (written < 0).then(io::Error::last_os_error);
    // SAFETY: close(2) of the descriptor just opened.
    unsafe { libc::close(fd) };
    failed.map_or(Ok(()), Err)
}

/// An IPv4 socket of `kind`, closed on exec.
fn socket(kind: libc::c_int) -> io::Result<RawFd> {
    // SAFETY: socket(2) with plain integer arguments.
    check(unsafe { libc::socket(libc::AF_INET, kind | libc::SOCK_CLOEXEC, 0) })
}

fn loopback_up() -> io::Result<()> {
    let fd = socket(libc::SOCK_DGRAM)?;
    // SAFETY: an all-zero ifreq is valid (a C struct of integers and arrays).
    let mut request: libc::ifreq = unsafe { std::mem::zeroed() };
    for (slot, byte) in request.ifr_name.iter_mut().zip(b"lo") {
        *slot = libc::c_char::try_from(*byte).unwrap_or_default();
    }
    // SAFETY: SIOCGIFFLAGS/SIOCSIFFLAGS read and write `request`, a live
    // ifreq naming `lo`; ifru_flags is the union member these use.
    let result = unsafe {
        let mut result = libc::ioctl(fd, GET_FLAGS, &raw mut request);
        if result == 0 {
            #[allow(clippy::cast_possible_truncation)]
            let up = libc::IFF_UP as libc::c_short;
            request.ifr_ifru.ifru_flags |= up;
            result = libc::ioctl(fd, SET_FLAGS, &raw const request);
        }
        result
    };
    let failed = (result < 0).then(io::Error::last_os_error);
    // SAFETY: close(2) of the socket just opened.
    unsafe { libc::close(fd) };
    failed.map_or(Ok(()), Err)
}

/// A listening socket on `127.0.0.1:port` (`0`: any port).
fn listen(port: u16) -> io::Result<RawFd> {
    let fd = socket(libc::SOCK_STREAM)?;
    // SAFETY: an all-zero sockaddr_in is valid; the fields are set below.
    let mut address: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    address.sin_family = libc::sa_family_t::try_from(libc::AF_INET).unwrap_or_default();
    address.sin_port = port.to_be();
    address.sin_addr.s_addr = u32::from(std::net::Ipv4Addr::LOCALHOST).to_be();
    let length =
        libc::socklen_t::try_from(std::mem::size_of::<libc::sockaddr_in>()).unwrap_or_default();
    // SAFETY: bind(2) with a live sockaddr_in and its size; listen(2).
    let result = unsafe {
        let bound = libc::bind(fd, (&raw const address).cast(), length);
        if bound == 0 {
            libc::listen(fd, 128)
        } else {
            bound
        }
    };
    if result < 0 {
        let err = io::Error::last_os_error();
        // SAFETY: close(2) of the socket just opened.
        unsafe { libc::close(fd) };
        return Err(err);
    }
    Ok(fd)
}

/// Room for one `SCM_RIGHTS` message carrying one descriptor (aligned
/// for `cmsghdr`).
#[repr(C, align(8))]
struct Control([u8; 64]);

/// Sends `fd` over socket `channel` (one byte of payload).
fn send_fd(channel: RawFd, fd: RawFd) -> io::Result<()> {
    let mut byte = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: 1,
    };
    let mut control = Control([0; 64]);
    // SAFETY: an all-zero msghdr is valid; the pointers set below outlive
    // the sendmsg(2) call, and the control buffer has room for one
    // descriptor (CMSG_SPACE(4) <= 64).
    unsafe {
        let mut message: libc::msghdr = std::mem::zeroed();
        message.msg_iov = &raw mut iov;
        message.msg_iovlen = 1;
        message.msg_control = control.0.as_mut_ptr().cast();
        message.msg_controllen = libc::CMSG_SPACE(fd_size()) as _;
        let header = libc::CMSG_FIRSTHDR(&raw const message);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(fd_size()) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(header).cast::<RawFd>(), fd);
        check(
            libc::sendmsg(channel, &raw const message, 0)
                .try_into()
                .unwrap_or(-1),
        )?;
    }
    Ok(())
}

/// Receives the descriptor [`send_fd`] sent; `None` when the child closed
/// the channel without sending one (the spawn failed or never happened).
fn recv_fd(channel: &UnixStream) -> io::Result<Option<TcpListener>> {
    let mut byte = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: 1,
    };
    let mut control = Control([0; 64]);
    // SAFETY: as in `send_fd`; the received descriptor is taken over by an
    // OwnedFd exactly once.
    unsafe {
        let mut message: libc::msghdr = std::mem::zeroed();
        message.msg_iov = &raw mut iov;
        message.msg_iovlen = 1;
        message.msg_control = control.0.as_mut_ptr().cast();
        message.msg_controllen = libc::CMSG_SPACE(fd_size()) as _;
        let received = libc::recvmsg(
            channel.as_raw_fd(),
            &raw mut message,
            libc::MSG_CMSG_CLOEXEC,
        );
        if received < 0 {
            return Err(io::Error::last_os_error());
        }
        let header = libc::CMSG_FIRSTHDR(&raw const message);
        if received == 0
            || header.is_null()
            || (*header).cmsg_level != libc::SOL_SOCKET
            || (*header).cmsg_type != libc::SCM_RIGHTS
        {
            return Ok(None);
        }
        let fd = std::ptr::read_unaligned(libc::CMSG_DATA(header).cast::<RawFd>());
        Ok(Some(TcpListener::from(OwnedFd::from_raw_fd(fd))))
    }
}

/// Lets `channel` survive `exec`, so the child and its descendants hold
/// it while they live.
fn inherit(channel: RawFd) -> io::Result<()> {
    // SAFETY: fcntl(2) F_SETFD on an open descriptor.
    check(unsafe { libc::fcntl(channel, libc::F_SETFD, 0) }).map(|_| ())
}

// glibc's ioctl(2) takes an unsigned long request and musl's an int; the
// SIOC* numbers (0x89xx) fit both, so the cast never truncates.
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
const GET_FLAGS: libc::Ioctl = libc::SIOCGIFFLAGS as libc::Ioctl;
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
const SET_FLAGS: libc::Ioctl = libc::SIOCSIFFLAGS as libc::Ioctl;

#[allow(clippy::cast_possible_truncation)]
const fn fd_size() -> libc::c_uint {
    std::mem::size_of::<RawFd>() as libc::c_uint
}

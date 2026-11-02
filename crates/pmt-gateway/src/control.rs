//! The daemon's end of the control channel (spec: workspace-architecture,
//! Daemon discovery file and local control channel): a Unix domain socket
//! `<home>/run/control.sock` (mode 0600 in the 0700 `run/` directory, or in
//! an owner-only directory under `/tmp` when the home's path is too long
//! for a socket; a peer whose uid differs from the daemon's is refused), or
//! on Windows a named pipe that rejects remote clients. One JSON line in,
//! one out.

use std::path::Path;
use std::sync::Arc;

use pmt_core::error::{Error, codes};
use pmt_rpc::control::{ControlError, ControlRequest, ControlResponse};
use tokio::io::{AsyncBufReadExt as _, AsyncRead, AsyncWrite, AsyncWriteExt as _, BufReader};
use tracing::Instrument as _;

use crate::server::Server;
use crate::tickets::Grant;

/// The longest Unix socket path every supported OS accepts (macOS allows
/// 103 bytes, Linux 107).
#[cfg(unix)]
const MAX_SOCKET_PATH: usize = 100;

/// A short name for `home` (pipe names, relocated sockets).
fn home_hash(home: &Path) -> String {
    use sha2::{Digest as _, Sha256};
    let hash = Sha256::digest(home.to_string_lossy().as_bytes());
    crate::tickets::hex(&hash[..8])
}

/// The control channel's address for `home`: `<home>/run/control.sock`,
/// or, when that path is too long for a Unix socket, `<hash>.sock` in the
/// owner-only directory `/tmp/prometheus-<uid>`.
///
/// # Errors
///
/// `home.io` when the relocated socket's directory cannot be made private.
pub fn address(home: &Path) -> Result<String, Error> {
    #[cfg(unix)]
    {
        address_under(home, Path::new("/tmp"))
    }
    #[cfg(windows)]
    {
        Ok(format!(r"\\.\pipe\prometheus-{}", home_hash(home)))
    }
}

/// [`address`], relocating long paths under `short_root`.
#[cfg(unix)]
fn address_under(home: &Path, short_root: &Path) -> Result<String, Error> {
    let run = pmt_core::runtime::run_dir(home);
    let path = run.join("control.sock");
    if path.as_os_str().len() <= MAX_SOCKET_PATH {
        return Ok(path.to_string_lossy().into_owned());
    }
    let dir = private_socket_dir(&run, short_root)?;
    Ok(dir
        .join(format!("{}.sock", home_hash(home)))
        .to_string_lossy()
        .into_owned())
}

/// `<short_root>/prometheus-<uid>`, created 0700; refused unless it is a real
/// directory owned by the owner of `run` (the home's `run/`) with no
/// access for anyone else.
#[cfg(unix)]
fn private_socket_dir(run: &Path, short_root: &Path) -> Result<std::path::PathBuf, Error> {
    use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _};
    let io = |path: &Path, e: &std::io::Error| Error::io(&codes::HOME_IO, path, e);
    std::fs::create_dir_all(run).map_err(|e| io(run, &e))?;
    let uid = std::fs::metadata(run).map_err(|e| io(run, &e))?.uid();
    let dir = short_root.join(format!("prometheus-{uid}"));
    match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
        Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => return Err(io(&dir, &e)),
        _ => {}
    }
    let meta = std::fs::symlink_metadata(&dir).map_err(|e| io(&dir, &e))?;
    if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o077 != 0 {
        return Err(Error::new(
            &codes::HOME_IO,
            format!(
                "{} is not a private directory of this user; remove it and retry",
                dir.display()
            ),
        ));
    }
    Ok(dir)
}

/// Answers one request. `peer_pid` is the connecting process as the
/// kernel reports it; it wins over the pid the client claims, which any
/// same-user process could forge. The claimed pid is used only where the
/// OS cannot tell (Windows named pipes).
pub(crate) fn answer(
    server: &Server,
    request: ControlRequest,
    peer_pid: Option<u32>,
) -> ControlResponse {
    match request {
        ControlRequest::Ticket {
            purpose,
            profiles,
            pid,
        } => {
            let pid = peer_pid.or(pid);
            if let Some(only) = server.only_pid()
                && pid != Some(only)
            {
                let err = Error::new(
                    &codes::RPC_TICKET,
                    format!("this gateway serves only the TUI that runs it (pid {only})"),
                )
                .with_hint("it was started with 'prometheus tui --standalone'");
                return failure(&err);
            }
            let grant = Grant {
                profiles,
                purpose,
                pid,
            };
            match server.tickets.issue(grant, server.profiles()) {
                Ok(ticket) => ControlResponse {
                    ticket: Some(ticket),
                    ..ControlResponse::default()
                },
                Err(err) => failure(&err),
            }
        }
        ControlRequest::Status => ControlResponse {
            status: Some(server.status()),
            ..ControlResponse::default()
        },
        ControlRequest::Stop { restart } => {
            server.request_stop(restart);
            ControlResponse::default()
        }
    }
}

fn failure(err: &Error) -> ControlResponse {
    ControlResponse {
        error: Some(ControlError {
            code: err.code().code.to_owned(),
            message: err.message().to_owned(),
        }),
        ..ControlResponse::default()
    }
}

/// Reads one request line from `stream` and writes the answer.
async fn handle<S: AsyncRead + AsyncWrite + Unpin>(
    server: Arc<Server>,
    stream: S,
    peer_pid: Option<u32>,
) {
    let mut stream = BufReader::new(stream);
    let mut line = String::new();
    let read = tokio::time::timeout(pmt_rpc::control::TIMEOUT, stream.read_line(&mut line)).await;
    if !matches!(read, Ok(Ok(n)) if n > 0) {
        return;
    }
    let response = match serde_json::from_str::<ControlRequest>(&line) {
        Ok(request) => answer(&server, request, peer_pid),
        Err(e) => failure(&Error::new(
            &codes::RPC_INVALID_REQUEST,
            format!("bad control request: {e}"),
        )),
    };
    let mut text = serde_json::to_string(&response).unwrap_or_default();
    text.push('\n');
    let _ = stream.get_mut().write_all(text.as_bytes()).await;
    let _ = stream.get_mut().flush().await;
}

/// A bound control channel.
#[derive(Debug)]
pub struct Listener {
    #[cfg(unix)]
    inner: tokio::net::UnixListener,
    #[cfg(windows)]
    first: Option<tokio::net::windows::named_pipe::NamedPipeServer>,
    address: String,
}

impl Listener {
    /// Binds the control channel at `address`.
    ///
    /// # Errors
    ///
    /// `home.io` when the socket or pipe cannot be created.
    pub fn bind(address: &str) -> Result<Self, Error> {
        let failed = |e: &std::io::Error| {
            Error::new(
                &codes::HOME_IO,
                format!("cannot create the control channel {address}: {e}"),
            )
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            // Only the lock holder gets here, so a leftover socket is a
            // dead daemon's.
            let _ = std::fs::remove_file(address);
            let inner = tokio::net::UnixListener::bind(address).map_err(|e| failed(&e))?;
            std::fs::set_permissions(address, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| failed(&e))?;
            Ok(Self {
                inner,
                address: address.to_owned(),
            })
        }
        #[cfg(windows)]
        {
            let first = pipe_options(true).create(address).map_err(|e| failed(&e))?;
            Ok(Self {
                first: Some(first),
                address: address.to_owned(),
            })
        }
    }

    /// Serves requests until `server` is closing; removes the socket.
    pub async fn serve(mut self, server: Arc<Server>) {
        let closing = server.closing.clone();
        tokio::select! {
            () = closing.cancelled() => {}
            () = self.accept_loop(&server) => {}
        }
        #[cfg(unix)]
        let _ = std::fs::remove_file(&self.address);
    }

    #[cfg(unix)]
    async fn accept_loop(&mut self, server: &Arc<Server>) {
        let own_uid =
            std::fs::metadata(&self.address).map(|m| std::os::unix::fs::MetadataExt::uid(&m));
        loop {
            let Ok((stream, _)) = self.inner.accept().await else {
                continue;
            };
            let cred = stream.peer_cred();
            let peer_uid = cred.as_ref().map(tokio::net::unix::UCred::uid);
            if !matches!((&own_uid, &peer_uid), (Ok(own), Ok(peer)) if own == peer) {
                tracing::warn!("refused a control connection from another user");
                continue;
            }
            let kernel_pid = cred
                .ok()
                .and_then(|c| c.pid())
                .and_then(|p| u32::try_from(p).ok());
            tokio::spawn(handle(Arc::clone(server), stream, kernel_pid).in_current_span());
        }
    }

    #[cfg(windows)]
    async fn accept_loop(&mut self, server: &Arc<Server>) {
        let mut next = match self.first.take() {
            Some(first) => first,
            None => return,
        };
        loop {
            if next.connect().await.is_err() {
                continue;
            }
            let Ok(following) = pipe_options(false).create(&self.address) else {
                return;
            };
            let connected = std::mem::replace(&mut next, following);
            // tokio exposes no client process id for a named pipe (it needs
            // GetNamedPipeClientProcessId, unsafe Win32): the claimed pid
            // is used, a known gap like the pipe's default DACL.
            tokio::spawn(handle(Arc::clone(server), connected, None).in_current_span());
        }
    }
}

#[cfg(windows)]
fn pipe_options(first: bool) -> tokio::net::windows::named_pipe::ServerOptions {
    let mut options = tokio::net::windows::named_pipe::ServerOptions::new();
    options
        .first_pipe_instance(first)
        .reject_remote_clients(true);
    options
}

#[cfg(all(test, unix))]
mod tests {
    use super::address_under;

    #[test]
    fn a_home_too_deep_for_a_socket_gets_a_private_short_one() {
        let base = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let short = address_under(base.path(), root.path()).unwrap();
        assert!(short.ends_with("/run/control.sock"), "{short}");
        let deep = base.path().join("d".repeat(120));
        let relocated = address_under(&deep, root.path()).unwrap();
        let expected_dir = root.path().join("prometheus-");
        assert!(
            relocated.starts_with(expected_dir.to_str().unwrap()),
            "{relocated}"
        );
        assert_eq!(
            address_under(&deep, root.path()).unwrap(),
            relocated,
            "stable per home"
        );
        let dir = std::path::Path::new(&relocated).parent().unwrap();
        let mode =
            std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(dir).unwrap().permissions());
        assert_eq!(mode & 0o077, 0);
    }
}

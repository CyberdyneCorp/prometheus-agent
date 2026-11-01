//! The daemon's end of the control channel (spec: workspace-architecture,
//! Daemon discovery file and local control channel): a Unix domain socket
//! `<home>/run/control.sock` (mode 0600 in the 0700 `run/` directory, and a
//! peer whose uid differs from the daemon's is refused), or on Windows a
//! named pipe that rejects remote clients. One JSON line in, one out.

use std::path::Path;
use std::sync::Arc;

use pmt_core::error::{Error, codes};
use pmt_rpc::control::{ControlError, ControlRequest, ControlResponse};
use tokio::io::{AsyncBufReadExt as _, AsyncRead, AsyncWrite, AsyncWriteExt as _, BufReader};

use crate::server::Server;

/// The control channel's address for `home`.
#[must_use]
pub fn address(home: &Path) -> String {
    #[cfg(unix)]
    {
        pmt_core::runtime::run_dir(home)
            .join("control.sock")
            .to_string_lossy()
            .into_owned()
    }
    #[cfg(windows)]
    {
        use sha2::{Digest as _, Sha256};
        let hash = Sha256::digest(home.to_string_lossy().as_bytes());
        let short = crate::tickets::hex(&hash[..8]);
        format!(r"\\.\pipe\prometheus-{short}")
    }
}

/// Answers one request.
pub(crate) fn answer(server: &Server, request: ControlRequest) -> ControlResponse {
    match request {
        ControlRequest::Ticket { purpose, profiles } => {
            match server.tickets.issue(purpose, profiles, server.profiles()) {
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
async fn handle<S: AsyncRead + AsyncWrite + Unpin>(server: Arc<Server>, stream: S) {
    let mut stream = BufReader::new(stream);
    let mut line = String::new();
    let read = tokio::time::timeout(pmt_rpc::control::TIMEOUT, stream.read_line(&mut line)).await;
    if !matches!(read, Ok(Ok(n)) if n > 0) {
        return;
    }
    let response = match serde_json::from_str::<ControlRequest>(&line) {
        Ok(request) => answer(&server, request),
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
            let peer_uid = stream.peer_cred().map(|c| c.uid());
            if !matches!((&own_uid, &peer_uid), (Ok(own), Ok(peer)) if own == peer) {
                tracing::warn!("refused a control connection from another user");
                continue;
            }
            tokio::spawn(handle(Arc::clone(server), stream));
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
            tokio::spawn(handle(Arc::clone(server), connected));
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

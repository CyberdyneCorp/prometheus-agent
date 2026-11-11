//! The egress relay of a proxied child (spec: security-boundaries, Egress
//! enforcement for child processes).
//!
//! An `AppContainer` reaches a loopback listener only when the listener
//! belongs to the same container package, so the loopback egress proxy is
//! out of the child's reach. The launcher therefore starts a relay process
//! (a copy of the launcher's own executable run as `<exe> __pmt-sandbox-relay
//! <pipe>`) inside the child's container, in a Job Object of its own. The
//! relay listens on an ephemeral loopback port and forwards every
//! connection over a named pipe (see [`super::pipe`]) to the launcher,
//! which connects it to the proxy. The child holds no network capability,
//! so the relay is the only address it can reach and every connection
//! meets the proxy's host rules. The first pipe connection is the relay's
//! control channel: it carries the relay's port, and the relay exits when
//! the launcher closes it.

use std::ffi::{OsStr, OsString};
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _, copy_bidirectional};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient, NamedPipeServer};
use tokio::net::{TcpListener, TcpStream};
use tokio::runtime::{Builder, Runtime};

use super::launch::{Contained, Sid, spawn_contained};
use super::pipe::Security;

/// The first argument that turns an executable into a relay.
pub(crate) const RELAY_ARG: &str = "__pmt-sandbox-relay";

/// How long the relay may take to report its port.
const START_TIMEOUT: Duration = Duration::from_secs(10);

/// `ERROR_PIPE_BUSY`: every server instance is taken for the moment.
const PIPE_BUSY: i32 = 231;

/// A running relay; dropping it ends the relay process.
pub(super) struct Running {
    /// The relay's loopback port, the child's proxy address.
    pub(super) port: u16,
    _process: Contained,
    _runtime: Runtime,
}

/// Starts the relay of `container`'s child, forwarding to the egress
/// proxy on loopback `proxy_port`.
pub(super) fn start(container: &Sid, proxy_port: u16) -> Result<Running, String> {
    let name = OsString::from(format!(
        r"\\.\pipe\prometheus-egress-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    ));
    let security = Arc::new(Security::for_container(container.as_ptr())?);
    let runtime = Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|e| format!("egress relay runtime: {e}"))?;
    let mut control = {
        let _context = runtime.enter();
        security
            .create(&name, true)
            .map_err(|e| format!("egress relay pipe: {e}"))?
    };
    let exe = std::env::current_exe().map_err(|e| format!("egress relay executable: {e}"))?;
    let process = spawn_contained(
        container,
        exe.as_os_str(),
        &[OsString::from(RELAY_ARG), name.clone()],
    )
    .map_err(|e| format!("egress relay: {e}"))?;
    let port = runtime
        .block_on(async {
            tokio::time::timeout(START_TIMEOUT, handshake(&mut control))
                .await
                .map_err(|_| "the egress relay did not start".to_owned())?
        })
        .map_err(|e| format!("egress relay: {e}"))?;
    pmt_core::spawn::spawn_on(
        runtime.handle(),
        forward(control, name, security, proxy_port),
    );
    Ok(Running {
        port,
        _process: process,
        _runtime: runtime,
    })
}

/// Waits for the relay on the control instance and reads its port.
async fn handshake(control: &mut NamedPipeServer) -> Result<u16, String> {
    control.connect().await.map_err(|e| e.to_string())?;
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while line.len() < 16 {
        let read = control.read(&mut byte).await.map_err(|e| e.to_string())?;
        if read == 0 || byte[0] == b'\n' {
            break;
        }
        line.push(byte[0]);
    }
    let text = String::from_utf8_lossy(&line);
    text.parse()
        .map_err(|_| format!("unexpected answer {text:?}"))
}

/// Serves each further pipe connection by connecting it to the proxy,
/// keeping `control` open (the relay's life) for as long as it runs.
async fn forward(control: NamedPipeServer, name: OsString, security: Arc<Security>, port: u16) {
    let _control = control;
    loop {
        let Ok(server) = security.create(&name, false) else {
            return;
        };
        if server.connect().await.is_err() {
            continue;
        }
        pmt_core::spawn::spawn(async move {
            let mut server = server;
            if let Ok(mut proxy) = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).await {
                let _ = copy_bidirectional(&mut server, &mut proxy).await;
            }
        });
    }
}

/// The relay process: listens on loopback inside the container, reports
/// its port over `pipe`, and forwards each connection over a new pipe
/// connection until the launcher closes the control channel.
pub(crate) fn serve(pipe: &OsStr) -> Result<i32, String> {
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    runtime.block_on(async {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|e| format!("relay listener: {e}"))?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        let mut control = open(pipe).await.map_err(|e| format!("relay pipe: {e}"))?;
        control
            .write_all(format!("{port}\n").as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        let mut closed = [0u8; 1];
        loop {
            tokio::select! {
                _ = control.read(&mut closed) => return Ok(0),
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { continue };
                    pmt_core::spawn::spawn(relay(stream, pipe.to_os_string()));
                }
            }
        }
    })
}

async fn relay(mut stream: TcpStream, pipe: OsString) {
    if let Ok(mut launcher) = open(&pipe).await {
        let _ = copy_bidirectional(&mut stream, &mut launcher).await;
    }
}

/// Opens a client end of `pipe`, waiting while every instance is busy.
async fn open(pipe: &OsStr) -> std::io::Result<NamedPipeClient> {
    let deadline = tokio::time::Instant::now() + START_TIMEOUT;
    loop {
        match ClientOptions::new().open(pipe) {
            Err(e)
                if e.raw_os_error() == Some(PIPE_BUSY)
                    && tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            other => return other,
        }
    }
}

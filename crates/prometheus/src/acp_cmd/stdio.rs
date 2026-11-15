//! The protocol stream of `prometheus acp` (spec: acp-adapter, Stdout
//! carries only protocol frames): on Unix the process keeps a private
//! duplicate of its stdout for the frames and points file descriptor 1 at
//! stderr, so a stray print of any component, and the stdout of every
//! child process (which inherits descriptor 1), lands on stderr instead of
//! corrupting the protocol. Elsewhere the frames use stdout directly; the
//! children the adapter starts (the daemon) get a null stdout and logs go
//! to stderr.

use pmt_core::error::{Error, codes};
use tokio::io::AsyncWrite;

/// Where the adapter writes its frames.
pub(super) type Protocol = Box<dyn AsyncWrite + Unpin + Send>;

fn failed(e: &std::io::Error) -> Error {
    Error::new(
        &codes::OUTPUT_IO,
        format!("cannot set up the protocol stream: {e}"),
    )
}

/// The protocol stream; stdout itself now leads to stderr.
///
/// # Errors
///
/// `output.io` when the descriptors cannot be duplicated.
#[cfg(unix)]
pub(super) fn protocol_stdout() -> Result<Protocol, Error> {
    let private = nix::unistd::dup(std::io::stdout()).map_err(|e| failed(&e.into()))?;
    nix::unistd::dup2_stdout(std::io::stderr()).map_err(|e| failed(&e.into()))?;
    let file = std::fs::File::from(private);
    Ok(Box::new(tokio::fs::File::from_std(file)))
}

/// The protocol stream.
///
/// # Errors
///
/// Never on this platform.
#[cfg(not(unix))]
pub(super) fn protocol_stdout() -> Result<Protocol, Error> {
    let _ = failed;
    Ok(Box::new(tokio::io::stdout()))
}

//! A mail server connection: TLS from the first byte, plain upgraded with
//! STARTTLS, or plain (loopback hosts only), read line by line.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use pmt_config::platforms::MailSecurity;
use tokio::io::{
    AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, BufReader,
};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::pki_types::ServerName;

/// How long one read or write may take.
pub(crate) const IO_TIMEOUT: Duration = Duration::from_mins(1);

/// A byte stream, TLS or plain.
pub(crate) trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

/// A buffered connection.
pub(crate) struct Wire {
    stream: BufReader<Box<dyn Stream>>,
    host: String,
}

/// Why a connection failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WireError(pub(crate) String);

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn io(err: impl std::fmt::Display) -> WireError {
    WireError(err.to_string())
}

/// Whether `host` names this machine (plain connections are allowed only
/// there).
pub(crate) fn is_loopback(host: &str) -> bool {
    host == "localhost"
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

async fn tls(
    stream: Box<dyn Stream>,
    host: &str,
    extra_ca: Option<&Path>,
) -> Result<Box<dyn Stream>, WireError> {
    let config =
        pmt_net::tls::client_config(extra_ca).map_err(|e| WireError(e.message().to_owned()))?;
    let name = ServerName::try_from(host.to_owned())
        .map_err(|_| WireError(format!("{host} is not a valid TLS name")))?;
    let stream = TlsConnector::from(Arc::new(config))
        .connect(name, stream)
        .await
        .map_err(|e| WireError(format!("TLS with {host} failed: {e}")))?;
    Ok(Box::new(stream))
}

impl Wire {
    /// Connects to `host:port`; STARTTLS is the protocol's to request
    /// (see [`Self::upgrade`]).
    pub(crate) async fn connect(
        host: &str,
        port: u16,
        security: MailSecurity,
        extra_ca: Option<&Path>,
    ) -> Result<Self, WireError> {
        if security == MailSecurity::Plain && !is_loopback(host) {
            return Err(WireError(format!(
                "plain (unencrypted) mail connections are allowed only to loopback hosts, not {host}"
            )));
        }
        let tcp = tokio::time::timeout(IO_TIMEOUT, TcpStream::connect((host, port)))
            .await
            .map_err(|_| WireError(format!("{host}:{port} did not answer")))?
            .map_err(|e| WireError(format!("cannot reach {host}:{port}: {e}")))?;
        let stream: Box<dyn Stream> = if security == MailSecurity::Tls {
            tls(Box::new(tcp), host, extra_ca).await?
        } else {
            Box::new(tcp)
        };
        Ok(Self {
            stream: BufReader::new(stream),
            host: host.to_owned(),
        })
    }

    /// Switches the connection to TLS (after the server agreed to
    /// STARTTLS).
    pub(crate) async fn upgrade(self, extra_ca: Option<&Path>) -> Result<Self, WireError> {
        let host = self.host.clone();
        let stream = tls(self.stream.into_inner(), &host, extra_ca).await?;
        Ok(Self {
            stream: BufReader::new(stream),
            host,
        })
    }

    /// Writes `bytes`.
    pub(crate) async fn write(&mut self, bytes: &[u8]) -> Result<(), WireError> {
        let stream = self.stream.get_mut();
        tokio::time::timeout(IO_TIMEOUT, async {
            stream.write_all(bytes).await?;
            stream.flush().await
        })
        .await
        .map_err(|_| WireError("the server stopped reading".to_owned()))?
        .map_err(io)
    }

    /// Reads one line, without its line ending.
    pub(crate) async fn line(&mut self) -> Result<String, WireError> {
        let mut line = Vec::new();
        let read = tokio::time::timeout(IO_TIMEOUT, self.stream.read_until(b'\n', &mut line))
            .await
            .map_err(|_| WireError("the server stopped answering".to_owned()))?
            .map_err(io)?;
        if read == 0 {
            return Err(WireError("the server closed the connection".to_owned()));
        }
        while line.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
            line.pop();
        }
        Ok(String::from_utf8_lossy(&line).into_owned())
    }

    /// Reads exactly `len` bytes.
    pub(crate) async fn exact(&mut self, len: usize) -> Result<Vec<u8>, WireError> {
        let mut bytes = vec![0; len];
        tokio::time::timeout(IO_TIMEOUT, self.stream.read_exact(&mut bytes))
            .await
            .map_err(|_| WireError("the server stopped answering".to_owned()))?
            .map_err(io)?;
        Ok(bytes)
    }
}

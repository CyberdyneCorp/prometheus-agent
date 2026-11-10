//! A mail server connection: TLS from the first byte, plain upgraded with
//! STARTTLS, or plain (loopback hosts only), read line by line.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use pmt_config::platforms::MailSecurity;
use tokio::io::{
    AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, BufReader,
};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use tokio_rustls::rustls::crypto::{
    CryptoProvider, verify_tls12_signature, verify_tls13_signature,
};
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio_rustls::rustls::{self, DigitallySignedStruct, SignatureScheme};

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

/// How TLS connections are verified.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct TlsPolicy {
    /// `network.extra_ca_file`.
    pub(crate) extra_ca: Option<PathBuf>,
    /// `options.tls_verify`: verify the server certificate.
    pub(crate) verify: bool,
}

/// Accepts any certificate (`tls_verify = false`); handshake signatures
/// are still checked, so the connection is encrypted, just not
/// authenticated.
#[derive(Debug)]
struct Unverified(Arc<CryptoProvider>);

impl ServerCertVerifier for Unverified {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

fn client_config(policy: &TlsPolicy) -> Result<rustls::ClientConfig, WireError> {
    if policy.verify {
        return pmt_net::tls::client_config(policy.extra_ca.as_deref())
            .map_err(|e| WireError(e.message().to_owned()));
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    Ok(
        rustls::ClientConfig::builder_with_provider(Arc::clone(&provider))
            .with_safe_default_protocol_versions()
            .map_err(io)?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(Unverified(provider)))
            .with_no_client_auth(),
    )
}

async fn tls(
    stream: Box<dyn Stream>,
    host: &str,
    policy: &TlsPolicy,
) -> Result<Box<dyn Stream>, WireError> {
    let config = client_config(policy)?;
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
        policy: &TlsPolicy,
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
            tls(Box::new(tcp), host, policy).await?
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
    pub(crate) async fn upgrade(self, policy: &TlsPolicy) -> Result<Self, WireError> {
        let host = self.host.clone();
        let stream = tls(self.stream.into_inner(), &host, policy).await?;
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

#[cfg(test)]
mod tests {
    use tokio::io::AsyncWriteExt as _;
    use tokio_rustls::TlsAcceptor;
    use tokio_rustls::rustls::pki_types::PrivateKeyDer;

    use super::*;

    /// A TLS server with a self-signed certificate for `localhost` that
    /// greets once; its port.
    async fn self_signed_server() -> u16 {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let key = PrivateKeyDer::try_from(cert.signing_key.serialize_der()).unwrap();
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert.cert.der().clone()], key)
        .unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        pmt_core::spawn::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                pmt_core::spawn::spawn(async move {
                    if let Ok(mut tls) = acceptor.accept(tcp).await {
                        let _ = tls.write_all(b"* OK ready\r\n").await;
                        let _ = tls.flush().await;
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                });
            }
        });
        port
    }

    #[tokio::test]
    async fn certificates_are_verified_unless_tls_verify_is_off() {
        let port = self_signed_server().await;
        let verified = TlsPolicy {
            extra_ca: None,
            verify: true,
        };
        let refused = Wire::connect("localhost", port, MailSecurity::Tls, &verified).await;
        assert!(
            refused.as_ref().is_err_and(|e| e.0.contains("TLS")),
            "{:?}",
            refused.err()
        );
        let unverified = TlsPolicy {
            extra_ca: None,
            verify: false,
        };
        let mut wire = Wire::connect("localhost", port, MailSecurity::Tls, &unverified)
            .await
            .unwrap();
        assert_eq!(wire.line().await.unwrap(), "* OK ready");
    }
}

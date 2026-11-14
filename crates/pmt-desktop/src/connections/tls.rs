//! Dialing a `remote` connection (spec: desktop-app, Remote connections are
//! authenticated and encrypted): `https` verified against the system's
//! roots, or, for a self-signed server, against the SHA-256 public-key pin
//! the user confirmed after comparing it with what `prometheus dashboard
//! --fingerprint` prints on the server. A presented key that differs from
//! the pin is reported with both fingerprints and never accepted.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use url::Url;

/// How long a TCP connect and a TLS handshake may take.
pub const DIAL_TIMEOUT: Duration = Duration::from_secs(10);

/// A byte stream to a connection's server.
pub trait Stream: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> Stream for T {}

/// A boxed [`Stream`].
pub type BoxStream = Box<dyn Stream>;

/// Why a dial failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DialFailure {
    /// No pin, and the system's roots do not trust the certificate: the
    /// user compares `fingerprint` with the server's and pins it.
    Untrusted {
        /// The presented key's fingerprint.
        fingerprint: String,
    },
    /// The server presented another key than the pinned one.
    PinMismatch {
        /// The pin.
        pinned: String,
        /// What the server presented.
        presented: String,
    },
    /// Not reached (refused, timed out, a TLS error).
    Unreachable(String),
}

impl std::fmt::Display for DialFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Untrusted { fingerprint } => write!(
                f,
                "the server's certificate is not trusted by this system; compare {fingerprint} with `prometheus dashboard --fingerprint` on the server before pinning it"
            ),
            Self::PinMismatch { pinned, presented } => write!(
                f,
                "the server's certificate has changed: pinned {pinned}, presented {presented}"
            ),
            Self::Unreachable(why) => write!(f, "unreachable: {why}"),
        }
    }
}

/// The SHA-256 fingerprint of a certificate's public key, as `prometheus
/// dashboard --fingerprint` prints it (`sha256:AB:CD:...`).
#[must_use]
pub fn fingerprint(cert: &CertificateDer<'_>) -> Option<String> {
    let leaf = webpki::EndEntityCert::try_from(cert).ok()?;
    let digest = Sha256::digest(leaf.subject_public_key_info().as_ref());
    let hex: Vec<String> = digest.iter().map(|b| format!("{b:02X}")).collect();
    Some(format!("sha256:{}", hex.join(":")))
}

/// The host and port `url` dials.
///
/// # Errors
///
/// A URL without a host or a known port.
pub fn address(url: &Url) -> Result<(String, u16), String> {
    let host = url.host_str().ok_or("the URL has no host")?;
    let port = url.port_or_known_default().ok_or("the URL has no port")?;
    Ok((
        host.trim_start_matches('[')
            .trim_end_matches(']')
            .to_owned(),
        port,
    ))
}

async fn tcp(url: &Url) -> Result<TcpStream, DialFailure> {
    let (host, port) = address(url).map_err(DialFailure::Unreachable)?;
    tokio::time::timeout(DIAL_TIMEOUT, TcpStream::connect((host.as_str(), port)))
        .await
        .map_err(|_| DialFailure::Unreachable("timed out".to_owned()))?
        .map_err(|e| DialFailure::Unreachable(e.to_string()))
}

/// Dials `url`: plain TCP for `http` (loopback only, the registry makes
/// sure), else TLS checked against `pin` or the system's roots.
///
/// # Errors
///
/// [`DialFailure`].
pub async fn dial(url: &Url, pin: Option<&str>) -> Result<BoxStream, DialFailure> {
    let stream = tcp(url).await?;
    if url.scheme() == "http" {
        return Ok(Box::new(stream));
    }
    let (host, _) = address(url).map_err(DialFailure::Unreachable)?;
    let name = ServerName::try_from(host).map_err(|e| DialFailure::Unreachable(e.to_string()))?;
    let recorder = Arc::new(Pinned::new(pin.map(str::to_owned)));
    let config = match pin {
        Some(_) => pinned_config(&recorder)?,
        None => pmt_net::tls::client_config(None)
            .map_err(|e| DialFailure::Unreachable(e.to_string()))?,
    };
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let handshake = tokio::time::timeout(DIAL_TIMEOUT, connector.connect(name.clone(), stream))
        .await
        .map_err(|_| DialFailure::Unreachable("the TLS handshake timed out".to_owned()))?;
    match handshake {
        Ok(tls) => Ok(Box::new(tls)),
        Err(e) => Err(match (pin, recorder.seen()) {
            (Some(pinned), Some(presented)) if pinned != presented => DialFailure::PinMismatch {
                pinned: pinned.to_owned(),
                presented,
            },
            (None, _) if is_certificate_error(&e) => untrusted(url, name).await,
            _ => DialFailure::Unreachable(e.to_string()),
        }),
    }
}

fn is_certificate_error(e: &std::io::Error) -> bool {
    e.get_ref()
        .and_then(|inner| inner.downcast_ref::<rustls::Error>())
        .is_some_and(|e| matches!(e, rustls::Error::InvalidCertificate(_)))
}

/// The system refused the certificate: one more handshake only reads the
/// presented key's fingerprint (it never completes).
async fn untrusted(url: &Url, name: ServerName<'static>) -> DialFailure {
    let recorder = Arc::new(Pinned::new(None));
    let Ok(config) = pinned_config(&recorder) else {
        return DialFailure::Unreachable("the certificate is not trusted".to_owned());
    };
    let Ok(stream) = tcp(url).await else {
        return DialFailure::Unreachable("the certificate is not trusted".to_owned());
    };
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let _ = tokio::time::timeout(DIAL_TIMEOUT, connector.connect(name, stream)).await;
    match recorder.seen() {
        Some(fingerprint) => DialFailure::Untrusted { fingerprint },
        None => DialFailure::Unreachable("the certificate is not trusted".to_owned()),
    }
}

fn pinned_config(verifier: &Arc<Pinned>) -> Result<ClientConfig, DialFailure> {
    let provider = Arc::clone(&verifier.provider);
    Ok(ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| DialFailure::Unreachable(e.to_string()))?
        // Pinning is a verifier: the presented key must be the pinned one.
        .dangerous()
        .with_custom_certificate_verifier(Arc::clone(verifier) as Arc<dyn ServerCertVerifier>)
        .with_no_client_auth())
}

/// Accepts exactly the pinned public key (no pin: nothing, it only
/// records what was presented); signatures are still checked.
#[derive(Debug)]
struct Pinned {
    pin: Option<String>,
    seen: Mutex<Option<String>>,
    provider: Arc<CryptoProvider>,
}

impl Pinned {
    fn new(pin: Option<String>) -> Self {
        Self {
            pin,
            seen: Mutex::new(None),
            provider: Arc::new(rustls::crypto::ring::default_provider()),
        }
    }

    fn seen(&self) -> Option<String> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let presented = fingerprint(end_entity);
        presented.clone_into(&mut self.seen.lock().unwrap_or_else(PoisonError::into_inner));
        match (&self.pin, presented) {
            (Some(pin), Some(presented)) if *pin == presented => {
                Ok(ServerCertVerified::assertion())
            }
            _ => Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            )),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

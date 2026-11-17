//! Credential injection at the egress proxy (spec: security-boundaries,
//! Credential injection at the egress proxy).
//!
//! A sandboxed child holds a per-session placeholder instead of each
//! mapped secret. The proxy terminates the child's TLS with a leaf
//! certificate its own [`Authority`] signs for the requested host (the
//! child trusts that CA through its CA bundle), opens its own verified
//! TLS connection upstream, and swaps a placeholder in the request head
//! for the real value only when the host is one the secret is mapped to.
//! A placeholder bound for any other host, in the head or later in the
//! stream, ends the connection before it is forwarded: an `egress.denied`
//! with reason `credential_misdirected`. Plain HTTP never gets a real
//! value, and a tunnel that is not TLS is only scanned. Every request on
//! an intercepted connection is sent with `Connection: close`, so each
//! head the proxy reads is a request head.

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use pmt_core::secret::SecretString;
use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

use super::policy::{Allowed, Denial, Reason};
use super::{Rules, Target, read_head};

/// The CA certificate's file name in the CA directory.
pub const CA_CERT: &str = "ca.crt";
/// The CA key's file name (0600, never copied off the host).
pub const CA_KEY: &str = "ca.key";

/// The proxy's certificate authority: a CA certificate the children trust
/// and the leaf certificates it signs per host.
pub struct Authority {
    issuer: rcgen::Issuer<'static, rcgen::KeyPair>,
    cert: CertificateDer<'static>,
    leaves: Mutex<HashMap<String, Arc<rustls::ServerConfig>>>,
}

impl std::fmt::Debug for Authority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Authority").finish_non_exhaustive()
    }
}

/// The CA's fixed parameters: re-created identically on every load, so
/// the leaves a reloaded key signs chain to the stored certificate.
fn ca_params() -> rcgen::CertificateParams {
    let mut params = rcgen::CertificateParams::default();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "Prometheus egress CA");
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params.key_usages = vec![
        rcgen::KeyUsagePurpose::KeyCertSign,
        rcgen::KeyUsagePurpose::CrlSign,
    ];
    params
}

fn invalid(what: &str, err: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("{what}: {err}"))
}

impl Authority {
    /// The CA kept in `dir` (`ca.crt`, `ca.key`), created there when
    /// missing: the key 0600 and created exclusively (never through a
    /// link), the directory 0700.
    ///
    /// # Errors
    ///
    /// The directory or files cannot be created or read; a stored key or
    /// certificate does not parse.
    pub fn open(dir: &Path) -> io::Result<Self> {
        if let Some(parent) = dir.parent() {
            std::fs::create_dir_all(parent)?;
        }
        match pmt_core::private_fs::create_dir(dir) {
            Err(err) if err.kind() != io::ErrorKind::AlreadyExists => return Err(err),
            _ => {}
        }
        let key_path = dir.join(CA_KEY);
        let cert_path = dir.join(CA_CERT);
        if !key_path.exists() {
            let key = rcgen::KeyPair::generate().map_err(|e| invalid("CA key", e))?;
            let cert = ca_params()
                .self_signed(&key)
                .map_err(|e| invalid("CA certificate", e))?;
            let mut file = pmt_core::private_fs::new_file_options().open(&key_path)?;
            file.write_all_sync(key.serialize_pem().as_bytes())?;
            pmt_core::private_fs::write_atomic(&cert_path, cert.pem().as_bytes())?;
        }
        let key_pem = pmt_core::private_fs::read_no_follow(&key_path)?;
        let key = rcgen::KeyPair::from_pem(&key_pem).map_err(|e| invalid("CA key", e))?;
        let cert =
            CertificateDer::from_pem_file(&cert_path).map_err(|e| invalid("CA certificate", e))?;
        Ok(Self {
            issuer: rcgen::Issuer::new(ca_params(), key),
            cert,
            leaves: Mutex::default(),
        })
    }

    /// The TLS server configuration presenting a leaf certificate for
    /// `host` (made once per host).
    ///
    /// # Errors
    ///
    /// The leaf cannot be made.
    pub fn server_config(&self, host: &str) -> io::Result<Arc<rustls::ServerConfig>> {
        let mut leaves = self.leaves.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(config) = leaves.get(host) {
            return Ok(Arc::clone(config));
        }
        let key = rcgen::KeyPair::generate().map_err(|e| invalid("leaf key", e))?;
        let leaf = rcgen::CertificateParams::new(vec![host.to_owned()])
            .and_then(|params| params.signed_by(&key, &self.issuer))
            .map_err(|e| invalid("leaf certificate", e))?;
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| invalid("TLS", e))?
            .with_no_client_auth()
            .with_single_cert(
                vec![leaf.der().clone(), self.cert.clone()],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
            )
            .map_err(|e| invalid("TLS", e))?;
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let config = Arc::new(config);
        leaves.insert(host.to_owned(), Arc::clone(&config));
        Ok(config)
    }
}

/// Writes all of `bytes` and syncs (the CA key is written once).
trait WriteSync {
    fn write_all_sync(&mut self, bytes: &[u8]) -> io::Result<()>;
}

impl WriteSync for std::fs::File {
    fn write_all_sync(&mut self, bytes: &[u8]) -> io::Result<()> {
        std::io::Write::write_all(self, bytes)?;
        self.sync_all()
    }
}

/// A fresh placeholder for one secret of one session: `pmt-egress-`
/// and 32 random hex digits.
///
/// # Panics
///
/// When the system has no random source.
#[must_use]
pub fn placeholder() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("the system random source works");
    bytes
        .iter()
        .fold(String::from("pmt-egress-"), |mut out, b| {
            use std::fmt::Write as _;
            let _ = write!(out, "{b:02x}");
            out
        })
}

/// One mapped secret.
#[derive(Clone)]
pub struct Mapping {
    /// The secret's name.
    pub name: String,
    /// What the child holds instead.
    pub placeholder: String,
    /// The real value.
    pub value: SecretString,
    /// The hosts it may be sent to.
    pub hosts: Vec<Allowed>,
}

impl std::fmt::Debug for Mapping {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mapping")
            .field("name", &self.name)
            .field("hosts", &self.hosts)
            .finish_non_exhaustive()
    }
}

/// What the proxy injects, and with which certificates.
#[derive(Debug)]
pub struct Injection {
    /// The mapped secrets.
    pub mappings: Vec<Mapping>,
    /// Signs the leaves children see.
    pub authority: Arc<Authority>,
    /// Verifies the real upstream servers.
    pub upstream: Arc<rustls::ClientConfig>,
}

/// A request head carried a placeholder to a host its secret is not
/// mapped to.
#[derive(Debug, PartialEq, Eq)]
struct Misdirected;

impl Injection {
    /// The placeholders `host:port` may not receive.
    fn foreign(&self, host: &str, port: u16) -> Vec<Vec<u8>> {
        self.mappings
            .iter()
            .filter(|m| !m.hosts.iter().any(|a| a.admits(host, port)))
            .map(|m| m.placeholder.clone().into_bytes())
            .collect()
    }

    /// `head` for `host:port`: placeholders of secrets mapped there
    /// replaced by their values when `inject`, the connection closed after
    /// this request.
    fn rewrite(
        &self,
        head: &[u8],
        host: &str,
        port: u16,
        inject: bool,
    ) -> Result<Vec<u8>, Misdirected> {
        if self.foreign(host, port).iter().any(|p| contains(head, p)) {
            return Err(Misdirected);
        }
        let mut text = String::from_utf8_lossy(head).into_owned();
        if inject {
            for mapping in &self.mappings {
                text = text.replace(&mapping.placeholder, mapping.value.expose());
            }
        }
        Ok(close_connection(&text).into_bytes())
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

/// `head` with its `Connection` headers replaced by `Connection: close`.
fn close_connection(head: &str) -> String {
    let body = head.trim_end_matches("\r\n");
    let mut lines: Vec<&str> = body
        .split("\r\n")
        .filter(|line| !line.to_ascii_lowercase().starts_with("connection:"))
        .collect();
    lines.push("Connection: close");
    let mut out = lines.join("\r\n");
    out.push_str("\r\n\r\n");
    out
}

/// The refusal of a misdirected placeholder.
fn misdirected(rules: &Rules, target: &Target) -> Denial {
    Denial {
        host: target.host.clone(),
        port: target.port,
        requester: rules.requester.clone(),
        reason: Reason::CredentialMisdirected,
    }
}

async fn refuse<W: AsyncWrite + Unpin>(client: &mut W, host: &str) {
    let body = format!("Blocked: {host} is blocked by policy (credential misdirected)\n");
    let reply = format!(
        "HTTP/1.1 403 Forbidden\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = client.write_all(reply.as_bytes()).await;
    let _ = client.shutdown().await;
}

/// Refuses a plain request whose head or first bytes carry a placeholder
/// bound for a host its secret is not mapped to, before any connection.
pub(super) async fn screen(
    client: &mut TcpStream,
    target: &Target,
    rest: &[u8],
    rules: &Rules,
    injection: &Injection,
) -> Result<(), Option<Denial>> {
    let foreign = injection.foreign(&target.host, target.port);
    let carried = |bytes: &[u8]| foreign.iter().any(|p| contains(bytes, p));
    if target.connect || !(carried(&target.head) || carried(rest)) {
        return Ok(());
    }
    refuse(client, &target.host).await;
    Err(Some(misdirected(rules, target)))
}

/// Serves a connection the policy admitted to `upstream`, when the session
/// injects credentials.
pub(super) async fn serve(
    mut client: TcpStream,
    mut upstream: TcpStream,
    target: &Target,
    rest: &[u8],
    rules: &Rules,
    injection: &Injection,
) -> Result<(), Option<Denial>> {
    let foreign = injection.foreign(&target.host, target.port);
    let found = |found: bool| found.then(|| misdirected(rules, target));
    if !target.connect {
        // Plain HTTP ([`screen`] refused a foreign placeholder in its head):
        // never a real value.
        let head = injection
            .rewrite(&target.head, &target.host, target.port, false)
            .map_err(|_| None)?;
        upstream.write_all(&head).await.map_err(|_| None)?;
        upstream.write_all(rest).await.map_err(|_| None)?;
        return pipe(&mut client, &mut upstream, &foreign)
            .await
            .map_err(found);
    }
    client
        .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
        .await
        .map_err(|_| None)?;
    let mut first = [0u8; 1];
    let tls = rest.is_empty() && matches!(client.peek(&mut first).await, Ok(1)) && first[0] == 0x16; // a TLS handshake record
    if !tls {
        // Not TLS: nothing to inject into, only scanned.
        upstream.write_all(rest).await.map_err(|_| None)?;
        return pipe(&mut client, &mut upstream, &foreign)
            .await
            .map_err(found);
    }
    let acceptor = tokio_rustls::TlsAcceptor::from(
        injection
            .authority
            .server_config(&target.host)
            .map_err(|_| None)?,
    );
    let mut client = acceptor.accept(client).await.map_err(|_| None)?;
    let (head, rest) = read_head(&mut client).await.map_err(|()| None)?;
    // A misdirected placeholder is refused before the real server is even
    // greeted.
    let head = match injection.rewrite(&head, &target.host, target.port, true) {
        Ok(head) if !foreign.iter().any(|p| contains(&rest, p)) => head,
        _ => {
            refuse(&mut client, &target.host).await;
            return Err(Some(misdirected(rules, target)));
        }
    };
    let name = ServerName::try_from(target.host.clone()).map_err(|_| None)?;
    let connector = tokio_rustls::TlsConnector::from(Arc::clone(&injection.upstream));
    let mut upstream = connector.connect(name, upstream).await.map_err(|_| None)?;
    upstream.write_all(&head).await.map_err(|_| None)?;
    upstream.write_all(&rest).await.map_err(|_| None)?;
    pipe(&mut client, &mut upstream, &foreign)
        .await
        .map_err(found)
}

/// Relays `client` and `upstream` until both are done; `Err(true)` when
/// the client sent a `foreign` placeholder (never forwarded), `Err(false)`
/// on an I/O error.
async fn pipe<C, U>(client: &mut C, upstream: &mut U, foreign: &[Vec<u8>]) -> Result<(), bool>
where
    C: AsyncRead + AsyncWrite + Unpin,
    U: AsyncRead + AsyncWrite + Unpin,
{
    let (mut client_read, mut client_write) = tokio::io::split(client);
    let (mut upstream_read, mut upstream_write) = tokio::io::split(upstream);
    let up = scanned(&mut client_read, &mut upstream_write, foreign);
    let down = async {
        let copied = tokio::io::copy(&mut upstream_read, &mut client_write).await;
        let _ = client_write.shutdown().await;
        copied.map(|_| ()).map_err(|_| false)
    };
    tokio::pin!(up, down);
    tokio::select! {
        // The request is sent: the response still has to come back.
        sent = &mut up => {
            sent?;
            down.await
        }
        received = &mut down => received,
    }
}

/// Copies `from` to `to`, holding back the bytes a placeholder could still
/// start in, so a foreign placeholder is never written even when split
/// across reads.
async fn scanned<R, W>(from: &mut R, to: &mut W, foreign: &[Vec<u8>]) -> Result<(), bool>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let keep = foreign
        .iter()
        .map(Vec::len)
        .max()
        .unwrap_or(1)
        .saturating_sub(1);
    let mut pending: Vec<u8> = Vec::new();
    // On the heap: the future stays small.
    let mut chunk = vec![0u8; 16 * 1024];
    loop {
        let n = from.read(&mut chunk).await.map_err(|_| false)?;
        if n == 0 {
            to.write_all(&pending).await.map_err(|_| false)?;
            let _ = to.shutdown().await;
            return Ok(());
        }
        pending.extend_from_slice(&chunk[..n]);
        if foreign.iter().any(|p| contains(&pending, p)) {
            return Err(true);
        }
        let ready = pending.len().saturating_sub(keep);
        to.write_all(&pending[..ready]).await.map_err(|_| false)?;
        pending.drain(..ready);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn injection() -> Injection {
        let dir = tempfile::tempdir().unwrap();
        Injection {
            mappings: vec![Mapping {
                name: "GITHUB_TOKEN".to_owned(),
                placeholder: "pmt-egress-0123".to_owned(),
                value: SecretString::new("ghp_real"),
                hosts: vec![Allowed::parse("api.github.com")],
            }],
            authority: Arc::new(Authority::open(&dir.path().join("egress")).unwrap()),
            upstream: Arc::new(crate::tls::client_config(None).unwrap()),
        }
    }

    #[test]
    fn a_mapped_host_gets_the_value_and_any_other_host_is_refused() {
        let inj = injection();
        let head = b"GET /user HTTP/1.1\r\nHost: api.github.com\r\nAuthorization: Bearer pmt-egress-0123\r\nConnection: keep-alive\r\n\r\n";
        let rewritten = inj.rewrite(head, "api.github.com", 443, true).unwrap();
        let text = String::from_utf8(rewritten).unwrap();
        assert!(text.contains("Bearer ghp_real"), "{text}");
        assert!(text.ends_with("Connection: close\r\n\r\n"), "{text}");
        assert!(!text.to_ascii_lowercase().contains("keep-alive"));
        assert_eq!(
            inj.rewrite(head, "evil.example", 443, true),
            Err(Misdirected)
        );
        // Plain HTTP to the mapped host: the placeholder stays.
        let plain = inj.rewrite(head, "api.github.com", 80, false).unwrap();
        assert!(
            String::from_utf8(plain)
                .unwrap()
                .contains("pmt-egress-0123")
        );
    }

    #[tokio::test]
    async fn a_placeholder_split_across_reads_is_never_forwarded() {
        let foreign = vec![b"pmt-egress-0123".to_vec()];
        let (mut writer, mut reader) = tokio::io::duplex(64);
        let sender = pmt_core::spawn::spawn(async move {
            writer.write_all(b"body=pmt-eg").await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            writer.write_all(b"ress-0123&x=1").await.unwrap();
        });
        let mut out = Vec::new();
        let found = scanned(&mut reader, &mut out, &foreign).await;
        sender.await.unwrap();
        assert_eq!(found, Err(true));
        assert!(!contains(&out, b"pmt-egress"), "{out:?}");
    }

    #[test]
    fn the_authority_is_kept_and_reloaded_with_a_private_key() {
        let dir = tempfile::tempdir().unwrap();
        let ca = dir.path().join("egress");
        let first = Authority::open(&ca).unwrap();
        let again = Authority::open(&ca).unwrap();
        assert_eq!(first.cert, again.cert);
        again.server_config("api.github.com").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(ca.join(CA_KEY))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert!(placeholder().starts_with("pmt-egress-"));
        assert_ne!(placeholder(), placeholder());
    }
}

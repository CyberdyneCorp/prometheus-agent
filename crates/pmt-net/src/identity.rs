//! Client certificates for mutual TLS (spec: plugin-system, MCP mTLS and
//! identity header).
//!
//! A client identity is a PEM certificate chain and its private key: both
//! in one combined PEM file, or the key in a file of its own. An encrypted
//! PKCS#8 key (`ENCRYPTED PRIVATE KEY`) is decrypted with the given
//! password. A missing file fails naming it.

use std::path::Path;

use pmt_core::error::{Error, codes};
use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

/// A client certificate chain and its key.
#[derive(Debug)]
pub struct ClientIdentity {
    pub(crate) certs: Vec<CertificateDer<'static>>,
    pub(crate) key: PrivateKeyDer<'static>,
}

fn invalid(message: String) -> Error {
    Error::new(&codes::NET_CLIENT_INIT, message)
}

impl ClientIdentity {
    /// Reads the certificate chain from `cert` and the key from `key` (or
    /// from `cert` too), decrypting an encrypted key with `password`.
    ///
    /// # Errors
    ///
    /// `net.client_init`: `client certificate not found: <path>` for a
    /// missing file; no certificate or key; a key that cannot be
    /// decrypted.
    pub fn load(cert: &Path, key: Option<&Path>, password: Option<&str>) -> Result<Self, Error> {
        let key_path = key.unwrap_or(cert);
        for path in [cert, key_path] {
            if !path.is_file() {
                return Err(invalid(format!(
                    "client certificate not found: {}",
                    path.display()
                )));
            }
        }
        let read = |path: &Path| {
            std::fs::read(path).map_err(|e| invalid(format!("{}: {e}", path.display())))
        };
        let cert_pem = read(cert)?;
        let certs = CertificateDer::pem_slice_iter(&cert_pem)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| invalid(format!("{}: {e}", cert.display())))?;
        if certs.is_empty() {
            return Err(invalid(format!(
                "{}: no PEM certificate found",
                cert.display()
            )));
        }
        let key_pem = read(key_path)?;
        let key = private_key(&key_pem, password)
            .map_err(|why| invalid(format!("{}: {why}", key_path.display())))?;
        Ok(Self { certs, key })
    }
}

fn private_key(pem: &[u8], password: Option<&str>) -> Result<PrivateKeyDer<'static>, String> {
    let text = String::from_utf8_lossy(pem);
    if text.contains("-----BEGIN ENCRYPTED PRIVATE KEY-----") {
        let password = password.ok_or("the key is encrypted and no password was given")?;
        let begin = text
            .find("-----BEGIN ENCRYPTED PRIVATE KEY-----")
            .unwrap_or_default();
        let end = text[begin..]
            .find("-----END ENCRYPTED PRIVATE KEY-----")
            .map(|e| begin + e + "-----END ENCRYPTED PRIVATE KEY-----".len())
            .ok_or("unterminated ENCRYPTED PRIVATE KEY block")?;
        let (_, document) = pkcs8::der::SecretDocument::from_pem(&text[begin..end])
            .map_err(|e| format!("not an encrypted PKCS#8 key: {e}"))?;
        let encrypted = pkcs8::EncryptedPrivateKeyInfo::try_from(document.as_bytes())
            .map_err(|e| format!("not an encrypted PKCS#8 key: {e}"))?;
        let decrypted = encrypted
            .decrypt(password)
            .map_err(|e| format!("the key could not be decrypted (wrong password?): {e}"))?;
        return Ok(PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
            decrypted.as_bytes().to_vec(),
        )));
    }
    PrivateKeyDer::from_pem_slice(pem).map_err(|e| format!("no private key found: {e}"))
}

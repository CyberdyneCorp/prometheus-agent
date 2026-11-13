//! TLS certificates of the daemon's remote listeners: loading a PEM pair
//! into an acceptor, the self-signed certificate of the remote attachment
//! listener when none is configured, and the SHA-256 public-key
//! fingerprint clients pin (`prometheus dashboard --fingerprint`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pmt_core::error::{Error, codes};
use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use sha2::{Digest as _, Sha256};
use tokio_rustls::TlsAcceptor;

fn bad(what: &str, path: &Path, e: &dyn std::fmt::Display) -> Error {
    Error::new(
        &codes::CONFIG_INVALID,
        format!("{what}: cannot load {}: {e}", path.display()),
    )
}

fn chain(cert: &Path, what: &str) -> Result<Vec<CertificateDer<'static>>, Error> {
    let chain = CertificateDer::pem_file_iter(cert)
        .and_then(Iterator::collect::<Result<Vec<_>, _>>)
        .map_err(|e| bad(&format!("{what}.cert"), cert, &e))?;
    if chain.is_empty() {
        return Err(bad(
            &format!("{what}.cert"),
            cert,
            &"no certificate in the file",
        ));
    }
    Ok(chain)
}

/// The acceptor of the PEM pair `cert`/`key` (`what` names the config
/// table in errors).
///
/// # Errors
///
/// `config.invalid` naming the file that does not load.
pub fn acceptor(cert: &Path, key: &Path, what: &str) -> Result<TlsAcceptor, Error> {
    let chain = chain(cert, what)?;
    let key_der =
        PrivateKeyDer::from_pem_file(key).map_err(|e| bad(&format!("{what}.key"), key, &e))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let server = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .and_then(|b| b.with_no_client_auth().with_single_cert(chain, key_der))
        .map_err(|e| bad(&format!("{what}.cert"), cert, &e))?;
    Ok(TlsAcceptor::from(Arc::new(server)))
}

/// The SHA-256 fingerprint of the leaf certificate's public key
/// (`SubjectPublicKeyInfo`), as `sha256:` and colon-separated hex.
///
/// # Errors
///
/// `config.invalid` when the certificate does not load or parse.
pub fn fingerprint(cert: &Path, what: &str) -> Result<String, Error> {
    let chain = chain(cert, what)?;
    let leaf = webpki::EndEntityCert::try_from(&chain[0])
        .map_err(|e| bad(&format!("{what}.cert"), cert, &format!("{e:?}")))?;
    let digest = Sha256::digest(leaf.subject_public_key_info().as_ref());
    let hex: Vec<String> = digest.iter().map(|b| format!("{b:02X}")).collect();
    Ok(format!("sha256:{}", hex.join(":")))
}

/// The self-signed certificate and key of the remote attachment listener
/// under `<home>/run/`, created on first use.
///
/// # Errors
///
/// `home.io` when it cannot be written, `config.invalid` when generation
/// fails.
pub fn self_signed(home: &Path) -> Result<(PathBuf, PathBuf), Error> {
    let cert = home.join("run/remote-cert.pem");
    let key = home.join("run/remote-key.pem");
    if cert.is_file() && key.is_file() {
        return Ok((cert, key));
    }
    let io = |path: &Path, e: &dyn std::fmt::Display| {
        Error::new(&codes::HOME_IO, format!("{}: {e}", path.display()))
    };
    // Clients pin the key's fingerprint, so the names only need to parse.
    let names = vec!["localhost".to_owned()];
    let generated = rcgen::generate_simple_self_signed(names).map_err(|e| {
        Error::new(
            &codes::CONFIG_INVALID,
            format!("self-signed certificate: {e}"),
        )
    })?;
    if let Some(dir) = cert.parent() {
        std::fs::create_dir_all(dir).map_err(|e| io(dir, &e))?;
    }
    pmt_core::private_fs::write_atomic(&key, generated.signing_key.serialize_pem().as_bytes())
        .map_err(|e| io(&key, &e))?;
    pmt_core::private_fs::write_atomic(&cert, generated.cert.pem().as_bytes())
        .map_err(|e| io(&cert, &e))?;
    Ok((cert, key))
}

#[cfg(test)]
mod tests {
    use super::{acceptor, fingerprint, self_signed};

    #[test]
    fn a_self_signed_pair_loads_and_has_a_stable_fingerprint() {
        let home = tempfile::tempdir().unwrap();
        let (cert, key) = self_signed(home.path()).unwrap();
        assert!(acceptor(&cert, &key, "daemon.remote.tls").is_ok());
        let first = fingerprint(&cert, "daemon.remote.tls").unwrap();
        assert!(
            first.starts_with("sha256:") && first.len() == 7 + 32 * 3 - 1,
            "{first}"
        );
        let (again, _) = self_signed(home.path()).unwrap();
        assert_eq!(
            fingerprint(&again, "daemon.remote.tls").unwrap(),
            first,
            "kept, not regenerated"
        );
    }
}

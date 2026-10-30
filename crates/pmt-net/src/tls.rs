//! TLS for every outbound client (spec: security-boundaries, Platform trust
//! store for TLS): certificates are verified by the platform verifier
//! (macOS trust settings, Windows certificate store, Linux system bundle),
//! so a root the user installed in the OS (a corporate TLS-inspecting
//! proxy) is trusted without configuration, plus the PEM roots in
//! `network.extra_ca_file`. No setting disables verification.

use std::path::Path;
use std::sync::Arc;

use pmt_core::error::{Error, codes};
use rustls::ClientConfig;
use rustls::crypto::CryptoProvider;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject as _;
use rustls_platform_verifier::Verifier;

/// Environment variables through which sandboxed children receive the CA
/// bundle (Python requests, curl, Node and OpenSSL-based tools).
pub const CHILD_CA_ENV: [&str; 4] = [
    "SSL_CERT_FILE",
    "REQUESTS_CA_BUNDLE",
    "CURL_CA_BUNDLE",
    "NODE_EXTRA_CA_CERTS",
];

/// The rustls configuration of the shared HTTP client.
///
/// # Errors
///
/// `net.extra_ca` when `extra_ca_file` cannot be read or holds no
/// certificate; `net.client_init` when the platform verifier cannot start.
pub fn client_config(extra_ca_file: Option<&Path>) -> Result<ClientConfig, Error> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let extra = extra_ca_file.map(read_pem_certs).transpose()?;
    let verifier = platform_verifier(extra, provider.clone()).map_err(|e| {
        init_error(&format!(
            "cannot start the platform certificate verifier: {e}"
        ))
    })?;
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| init_error(&e.to_string()))?
        // A custom verifier is rustls' way to plug in the platform one;
        // it verifies, nothing is skipped.
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    Ok(config)
}

#[cfg(not(target_os = "android"))]
fn platform_verifier(
    extra: Option<Vec<CertificateDer<'static>>>,
    provider: Arc<CryptoProvider>,
) -> Result<Verifier, rustls::Error> {
    match extra {
        Some(roots) => Verifier::new_with_extra_roots(roots, provider),
        None => Verifier::new(provider),
    }
}

/// The Android verifier delegates to the system and takes no extra roots.
#[cfg(target_os = "android")]
fn platform_verifier(
    extra: Option<Vec<CertificateDer<'static>>>,
    provider: Arc<CryptoProvider>,
) -> Result<Verifier, rustls::Error> {
    if extra.is_some() {
        return Err(rustls::Error::General(
            "network.extra_ca_file is not supported on Android; install the root in the system store".into(),
        ));
    }
    Verifier::new(provider)
}

/// Reads every PEM certificate of `path`.
///
/// # Errors
///
/// `net.extra_ca` naming the file when it cannot be read, is not PEM or
/// holds no certificate.
pub fn read_pem_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>, Error> {
    let fail = |why: String| {
        Error::new(
            &codes::NET_EXTRA_CA,
            format!("network.extra_ca_file {}: {why}", path.display()),
        )
    };
    let certs = CertificateDer::pem_file_iter(path)
        .map_err(|e| fail(e.to_string()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| fail(e.to_string()))?;
    if certs.is_empty() {
        return Err(fail("no PEM certificate found".into()));
    }
    Ok(certs)
}

/// Writes the PEM bundle sandboxed children trust: the OS roots plus the
/// `network.extra_ca_file` roots, so children verify like the in-process
/// client does (the file is replaced atomically, mode 0600).
///
/// # Errors
///
/// `net.extra_ca` for a bad extra file; `net.client_init` when no root at
/// all could be collected or the bundle cannot be written.
pub fn write_child_bundle(dest: &Path, extra_ca_file: Option<&Path>) -> Result<(), Error> {
    let mut certs = rustls_native_certs::load_native_certs().certs;
    if let Some(extra) = extra_ca_file {
        certs.extend(read_pem_certs(extra)?);
    }
    if certs.is_empty() {
        return Err(init_error("no CA certificate found for the child bundle"));
    }
    let pem: String = certs.iter().map(|cert| pem_block(cert)).collect();
    pmt_core::private_fs::write_atomic(dest, pem.as_bytes()).map_err(|e| {
        init_error(&format!(
            "cannot write the CA bundle {}: {e}",
            dest.display()
        ))
    })
}

fn pem_block(cert: &CertificateDer<'_>) -> String {
    use base64::Engine as _;
    let body = base64::engine::general_purpose::STANDARD.encode(cert.as_ref());
    let mut out = String::from("-----BEGIN CERTIFICATE-----\n");
    for line in body.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(line).unwrap_or_default());
        out.push('\n');
    }
    out.push_str("-----END CERTIFICATE-----\n");
    out
}

/// The environment entries that hand `bundle` to a sandboxed child.
#[must_use]
pub fn child_ca_env(bundle: &Path) -> Vec<(&'static str, &Path)> {
    CHILD_CA_ENV.iter().map(|name| (*name, bundle)).collect()
}

fn init_error(message: &str) -> Error {
    Error::new(&codes::NET_CLIENT_INIT, message.to_owned())
}

#[cfg(test)]
mod tests {
    use super::{child_ca_env, client_config, read_pem_certs, write_child_bundle};

    #[test]
    fn extra_ca_file_must_hold_a_certificate() {
        let dir = tempfile::tempdir().unwrap();
        let empty = dir.path().join("empty.pem");
        std::fs::write(&empty, "not a certificate\n").unwrap();
        let err = read_pem_certs(&empty).unwrap_err();
        assert_eq!(err.code().code, "net.extra_ca");
        assert!(client_config(Some(&dir.path().join("missing.pem"))).is_err());

        let ca = rcgen::generate_simple_self_signed(vec!["ca.test".into()]).unwrap();
        let good = dir.path().join("ca.pem");
        std::fs::write(&good, ca.cert.pem()).unwrap();
        assert_eq!(read_pem_certs(&good).unwrap().len(), 1);
    }

    #[test]
    fn child_bundle_keeps_the_extra_roots() {
        let dir = tempfile::tempdir().unwrap();
        let ca = rcgen::generate_simple_self_signed(vec!["ca.test".into()]).unwrap();
        let extra = dir.path().join("corp.pem");
        std::fs::write(&extra, ca.cert.pem()).unwrap();
        let bundle = dir.path().join("bundle.pem");
        write_child_bundle(&bundle, Some(&extra)).unwrap();
        let certs = read_pem_certs(&bundle).unwrap();
        assert!(certs.iter().any(|c| c.as_ref() == ca.cert.der().as_ref()));
    }

    #[test]
    fn children_get_the_bundle_through_every_variable() {
        let bundle = std::path::Path::new("bundle.pem");
        let names: Vec<_> = child_ca_env(bundle).into_iter().map(|(n, _)| n).collect();
        assert_eq!(
            names,
            [
                "SSL_CERT_FILE",
                "REQUESTS_CA_BUNDLE",
                "CURL_CA_BUNDLE",
                "NODE_EXTRA_CA_CERTS"
            ]
        );
    }
}

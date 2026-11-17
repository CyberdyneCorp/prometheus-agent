//! A session's credential injection (spec: security-boundaries, Credential
//! injection at the egress proxy): with `security.egress.credential_injection`
//! on, every secret mapped in `security.egress.inject` that the profile
//! holds gets a fresh placeholder for this session. The session's children
//! hold the placeholders (their CA bundle also trusts the profile's egress
//! CA at `<profile>/egress/ca.crt`), and the session's proxy swaps them for
//! the real values on TLS requests to the mapped hosts only.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_net::egress::Allowed;
use pmt_net::egress::inject::{Authority, CA_CERT, Injection, Mapping, placeholder};

/// The injection of one session.
#[derive(Debug, Clone)]
pub struct SessionInjection {
    /// What the session's proxy injects.
    pub injection: Arc<Injection>,
    /// Secret name to the placeholder its children hold.
    pub placeholders: BTreeMap<String, String>,
    /// The CA certificate its children must trust.
    pub ca_cert: PathBuf,
}

/// The profile's egress CA directory.
#[must_use]
pub fn ca_dir(scope: &ProfileScope) -> PathBuf {
    scope.path("egress")
}

/// The injection of a new session of `scope`; `None` when injection is off
/// or maps nothing the profile holds.
///
/// # Errors
///
/// `net.client_init` when the CA or the upstream TLS configuration cannot
/// be set up: the session then does not start, rather than hand its
/// children real credentials.
pub fn for_session(scope: &ProfileScope) -> Result<Option<SessionInjection>, Error> {
    let config = scope.config();
    let egress = &config.security.egress;
    if !egress.credential_injection || egress.inject.is_empty() {
        return Ok(None);
    }
    let secrets = scope.secrets();
    let mappings: Vec<Mapping> = egress
        .inject
        .iter()
        .filter_map(|(name, hosts)| {
            let value = secrets.resolve(name).ok()?;
            Some(Mapping {
                name: name.clone(),
                placeholder: placeholder(),
                value,
                hosts: hosts.iter().map(|h| Allowed::parse(h)).collect(),
            })
        })
        .collect();
    if mappings.is_empty() {
        return Ok(None);
    }
    let failed = |what: &str, err: &dyn std::fmt::Display| {
        Error::new(
            &codes::NET_CLIENT_INIT,
            format!("credential injection is on but {what}: {err}"),
        )
    };
    let dir = ca_dir(scope);
    let authority = Authority::open(&dir).map_err(|e| {
        failed(
            &format!("the egress CA in {} is unusable", dir.display()),
            &e,
        )
    })?;
    let extra_ca = config
        .network
        .extra_ca_file
        .as_deref()
        .map(std::path::Path::new);
    let mut upstream = pmt_net::tls::client_config(extra_ca)?;
    upstream.alpn_protocols = vec![b"http/1.1".to_vec()];
    let placeholders = mappings
        .iter()
        .map(|m| (m.name.clone(), m.placeholder.clone()))
        .collect();
    Ok(Some(SessionInjection {
        injection: Arc::new(Injection {
            mappings,
            authority: Arc::new(authority),
            upstream: Arc::new(upstream),
        }),
        placeholders,
        ca_cert: dir.join(CA_CERT),
    }))
}

//! Credential injection for sandboxed children (spec: security-boundaries,
//! Credential injection at the egress proxy): with
//! `security.egress.credential_injection` on, every secret mapped in
//! `security.egress.inject` that the profile holds gets a fresh placeholder
//! per launch (a session's process children, a stdio MCP server, a python
//! plugin sidecar). The children hold the placeholders (their CA bundle
//! also trusts the profile's egress CA at `<profile>/egress/ca.crt`), and
//! the launch's own egress proxy swaps them for the real values on TLS
//! requests to the mapped hosts only.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_net::egress::Allowed;
use pmt_net::egress::inject::{Authority, CA_CERT, Injection, Mapping, placeholder};

/// The injection of one launch.
#[derive(Debug, Clone)]
pub struct ChildInjection {
    /// What the launch's proxy injects.
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

/// The injection of a new launch of children of `scope`; `None` when
/// injection is off or maps nothing the profile holds.
///
/// # Errors
///
/// `net.client_init` when the CA or the upstream TLS configuration cannot
/// be set up: the launch then fails, rather than hand its children real
/// credentials.
pub fn for_children(scope: &ProfileScope) -> Result<Option<ChildInjection>, Error> {
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
    Ok(Some(ChildInjection {
        injection: Arc::new(Injection {
            mappings,
            authority: Arc::new(authority),
            upstream: Arc::new(upstream),
        }),
        placeholders,
        ca_cert: dir.join(CA_CERT),
    }))
}

impl ChildInjection {
    /// What a child gets for secret `name` whose value is `value`: its
    /// placeholder when the secret is mapped, else the value itself.
    #[must_use]
    pub fn child_value<'a>(&'a self, name: &str, value: &'a str) -> &'a str {
        self.placeholders.get(name).map_or(value, String::as_str)
    }
}

/// What a child of a launch with `injection` gets for secret `name`.
#[must_use]
pub fn child_value<'a>(
    injection: Option<&'a ChildInjection>,
    name: &str,
    value: &'a str,
) -> &'a str {
    injection.map_or(value, |i| i.child_value(name, value))
}

/// Writes a sandboxed child's CA bundle to `dest`: the platform roots,
/// `network.extra_ca_file` and, when injecting, the egress CA (which
/// signs the hosts the proxy injects for).
///
/// # Errors
///
/// As [`pmt_net::tls::write_child_bundle`].
pub fn write_child_bundle(
    scope: &ProfileScope,
    injection: Option<&ChildInjection>,
    dest: &Path,
) -> Result<(), Error> {
    let config = scope.config();
    let mut extra_ca: Vec<&Path> = config
        .network
        .extra_ca_file
        .as_deref()
        .map(Path::new)
        .into_iter()
        .collect();
    if let Some(injection) = injection {
        extra_ca.push(&injection.ca_cert);
    }
    pmt_net::tls::write_child_bundle(dest, &extra_ca)
}

/// When injecting, writes the CA bundle of the child `command` starts to
/// `dest` and points the child at it (`SSL_CERT_FILE` and friends); a
/// failure stops the launch, since the child could not use its
/// placeholders.
///
/// # Errors
///
/// As [`write_child_bundle`].
pub fn trust_egress_ca(
    scope: &ProfileScope,
    injection: Option<&ChildInjection>,
    dest: &Path,
    command: &mut std::process::Command,
) -> Result<(), Error> {
    if injection.is_none() {
        return Ok(());
    }
    write_child_bundle(scope, injection, dest)?;
    for (name, path) in pmt_net::tls::child_ca_env(dest) {
        command.env(name, path);
    }
    Ok(())
}

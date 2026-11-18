//! How container and SSH sandboxes reach the egress proxy (spec:
//! security-boundaries, Egress proxy coverage of containers and remote
//! sandboxes): containers join the profile's egress-only network, whose
//! relay leads to the proxy's bridge listener; SSH hosts get a reverse
//! forward to its loopback listener. Both get the CA bundle and the
//! proxy variables; plugin remote sandboxes cannot reach the proxy and
//! singularity instances share the host's network, so both are refused
//! while it covers the profile.

use std::path::PathBuf;

use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};

use super::{covers, down, running};

/// What a container session needs to use the proxy.
#[derive(Debug, Clone)]
pub struct Cover {
    /// `http://<relay alias>:<bridge port>`.
    pub proxy_url: String,
    /// The egress-only network the container joins (`None` when the
    /// container has no network at all).
    pub network: Option<String>,
    /// The profile's egress CA certificate (mounted read-only).
    pub ca_cert: PathBuf,
    /// The CA bundle (platform roots and the egress CA; mounted
    /// read-only).
    pub bundle: PathBuf,
    /// The mapping file the placeholders are read from at each command.
    pub mappings: PathBuf,
}

impl Cover {
    /// The variables a container gets.
    #[must_use]
    pub fn env(&self, bundle: &str, ca: &str) -> Vec<(String, String)> {
        sandbox_env(&self.proxy_url, bundle, ca)
    }
}

/// The variables a sandbox gets: the proxy, the CA bundle and the
/// Node.js switches.
fn sandbox_env(proxy_url: &str, bundle: &str, ca: &str) -> Vec<(String, String)> {
    let mut vars: Vec<(String, String)> = pmt_net::egress::PROXY_VARS
        .iter()
        .map(|name| ((*name).to_owned(), proxy_url.to_owned()))
        .collect();
    for name in ["SSL_CERT_FILE", "REQUESTS_CA_BUNDLE", "CURL_CA_BUNDLE"] {
        vars.push((name.to_owned(), bundle.to_owned()));
    }
    vars.push(("NODE_EXTRA_CA_CERTS".to_owned(), ca.to_owned()));
    vars.push(("NODE_OPTIONS".to_owned(), "--use-openssl-ca".to_owned()));
    vars
}

/// The running proxy's record when a sandbox of `scope` must use it;
/// `None` when injection is off, or the proxy is down and not enforced.
fn needed(scope: &ProfileScope) -> Result<Option<super::Record>, Error> {
    if !covers(scope) {
        return Ok(None);
    }
    if let Some((record, _)) = running(scope) {
        return Ok(Some(record));
    }
    if scope.config().security.egress.enforce_on_container {
        return Err(down());
    }
    tracing::warn!(
        profile = %scope.name(),
        "the egress proxy is not running; this sandbox reaches the network without it"
    );
    Ok(None)
}

/// The CA bundle (platform roots, `network.extra_ca_file` and the egress
/// CA) and a copy of the egress CA, world-readable: a sandbox's user is
/// not ours, and these are public certificates (the key never leaves the
/// host).
fn ca_files(scope: &ProfileScope) -> Result<(PathBuf, PathBuf), Error> {
    let dir = super::super::injection::ca_dir(scope);
    let bundle = dir.join("container-ca-bundle.pem");
    let ca_cert = dir.join(pmt_net::egress::inject::CA_CERT);
    let config = scope.config();
    let mut extra: Vec<&std::path::Path> = config
        .network
        .extra_ca_file
        .as_deref()
        .map(std::path::Path::new)
        .into_iter()
        .collect();
    extra.push(&ca_cert);
    pmt_net::tls::write_child_bundle(&bundle, &extra)?;
    let ca_copy = dir.join("container-ca.crt");
    std::fs::copy(&ca_cert, &ca_copy).map_err(|e| Error::io(&codes::HOME_IO, &ca_copy, &e))?;
    for file in [&bundle, &ca_copy] {
        readable(file)?;
    }
    Ok((bundle, ca_copy))
}

/// The cover of a container session of `scope`: `None` when injection is
/// off.
///
/// # Errors
///
/// `egress.proxy_down` when the proxy is not running and
/// `security.egress.enforce_on_container` is on; the bundle's errors.
pub fn container_cover(
    scope: &ProfileScope,
    runtime: &pmt_sandbox::container::Runtime,
    image: &str,
) -> Result<Option<Cover>, Error> {
    let Some(record) = needed(scope)? else {
        return Ok(None);
    };
    // The container reaches nothing but the relay to the proxy.
    let egress = egress_network(scope, record.bridge.port()).reaching(record.relay_host);
    let network = if scope.config().terminal.container.network {
        egress.ensure(runtime, scope.name().as_str(), image)?;
        Some(egress.network.clone())
    } else {
        None
    };
    let (bundle, ca_cert) = ca_files(scope)?;
    Ok(Some(Cover {
        proxy_url: egress.proxy_url(),
        network,
        ca_cert,
        bundle,
        mappings: super::super::mappings::path(scope),
    }))
}

/// The tunnel of an SSH sandbox of `scope` to the proxy's loopback
/// listener, with the CA files to upload: `None` when injection is off.
///
/// # Errors
///
/// As [`container_cover`].
pub fn ssh_egress(
    scope: &ProfileScope,
) -> Result<Option<pmt_tools::terminal::remote::ssh::SshEgress>, Error> {
    let Some(record) = needed(scope)? else {
        return Ok(None);
    };
    let (bundle, ca) = ca_files(scope)?;
    let read =
        |path: &PathBuf| std::fs::read(path).map_err(|e| Error::io(&codes::HOME_IO, path, &e));
    let files = vec![
        ("ca-bundle.pem".to_owned(), read(&bundle)?),
        ("ca.crt".to_owned(), read(&ca)?),
    ];
    let mappings = super::super::mappings::path(scope);
    Ok(Some(pmt_tools::terminal::remote::ssh::SshEgress {
        proxy: record.loopback,
        files,
        vars: std::sync::Arc::new(move |proxy_url: &str, dir: &str| {
            let mut vars = sandbox_env(
                proxy_url,
                &format!("{dir}/ca-bundle.pem"),
                &format!("{dir}/ca.crt"),
            );
            vars.extend(super::super::mappings::read(&mappings));
            vars
        }),
    }))
}

/// Where a sandbox the proxy cannot cover runs.
#[derive(Debug, Clone, Copy)]
pub enum Uncovered {
    /// A plugin remote sandbox: its service cannot reach this host's
    /// proxy.
    Plugin,
    /// A singularity instance: it shares this host's network, so nothing
    /// keeps it to the proxy.
    Singularity,
}

/// Refuses a sandbox of `scope` the proxy cannot cover while credential
/// injection is on: its commands would either hold placeholders that
/// work nowhere or reach the network around the proxy.
///
/// # Errors
///
/// `egress.proxy_unreachable` naming the backend.
pub fn refuse_uncovered(scope: &ProfileScope, backend: &str, kind: Uncovered) -> Result<(), Error> {
    if !covers(scope) {
        return Ok(());
    }
    let why = match kind {
        Uncovered::Plugin => "runs outside this host and cannot reach its egress proxy",
        Uncovered::Singularity => {
            "shares this host's network, so the egress proxy cannot be enforced on it"
        }
    };
    Err(Error::new(
        &codes::EGRESS_PROXY_UNREACHABLE,
        format!("terminal backend '{backend}' {why}"),
    )
    .with_hint(
        "use the container or ssh backend, or turn security.egress.credential_injection off",
    ))
}

/// The egress-only network of `scope`'s containers.
fn egress_network(
    scope: &ProfileScope,
    port: u16,
) -> pmt_sandbox::container::egress::EgressNetwork {
    pmt_sandbox::container::egress::EgressNetwork::of(
        scope.name().as_str(),
        &scope.home().to_string_lossy(),
        port,
    )
}

/// Removes the relay of `scope`'s egress network once its proxy stopped
/// (only a profile on the container backend has one).
pub(super) fn remove_relay(scope: &ProfileScope) {
    let config = scope.config();
    if config.terminal.backend != "container" {
        return;
    }
    if let Ok(runtime) = pmt_sandbox::container::Runtime::detect(&config.terminal.container.runtime)
    {
        egress_network(scope, config.security.egress.proxy_port).remove_relay(&runtime);
    }
}

#[cfg(unix)]
fn readable(file: &std::path::Path) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o644))
        .map_err(|e| Error::io(&codes::HOME_IO, file, &e))
}

#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)] // fallible on Unix
fn readable(_file: &std::path::Path) -> Result<(), Error> {
    Ok(())
}

//! `daemon.toml [dashboard]` (spec: web-dashboard, Dashboard command and
//! listener defaults; Non-loopback binds fail closed; Reverse-proxy base
//! path from config; Upload and download limits): the daemon-wide web
//! dashboard listener. It is off until `prometheus dashboard` enables it,
//! binds loopback by default, and refuses any other address unless real
//! authentication and TLS are configured.

use std::net::IpAddr;
use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The default dashboard port.
pub const DEFAULT_PORT: u16 = 9120;
/// The default cap on one upload (25 MiB).
pub const DEFAULT_MAX_UPLOAD_BYTES: u64 = 25 * 1024 * 1024;
/// The secret (launch profile's store) holding the argon2id PHC hash of
/// the dashboard password (`dashboard.auth.mode = "password"`).
pub const PASSWORD_HASH_SECRET: &str = "DASHBOARD_PASSWORD_HASH";
/// The secret (launch profile's store) holding the deployment credential
/// of `dashboard.auth.mode = "token"`.
pub const SESSION_TOKEN_SECRET: &str = "DASHBOARD_SESSION_TOKEN";
/// The hosted account portal `dashboard.auth.portal.issuer` defaults to.
pub const DEFAULT_PORTAL_ISSUER: &str = "https://portal.nousresearch.com";

/// `daemon.toml [dashboard]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct DashboardConfig {
    /// The address the listener binds. Anything but a loopback address
    /// needs `auth.mode` and TLS.
    pub host: String,
    /// The port (`0`: chosen by the OS).
    pub port: u16,
    /// The path prefix the dashboard is served under (a reverse proxy's
    /// mount point, such as `/prometheus`).
    pub base_path: String,
    /// A host name accepted in the `Host` header besides the bound names.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_host: Option<String>,
    /// The address browsers reach the dashboard at (`https://dash.example.com`);
    /// OAuth callbacks are `<public_url>/auth/callback` when set, else derived
    /// from the request (trusted proxy headers included).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_url: Option<String>,
    /// Honour `X-Forwarded-*` headers from `trusted_proxies`.
    pub trust_proxy: bool,
    /// Proxy addresses whose `X-Forwarded-*` headers are honoured.
    pub trusted_proxies: Vec<String>,
    /// A trusted proxy terminates TLS in front of a plain listener.
    pub trust_proxy_tls: bool,
    /// The largest chat attachment upload.
    pub max_upload_bytes: u64,
    /// TLS served by the listener itself.
    pub tls: DashboardTls,
    /// How remote browsers sign in.
    pub auth: DashboardAuth,
}

impl Default for DashboardConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_owned(),
            port: DEFAULT_PORT,
            base_path: "/".to_owned(),
            public_host: None,
            public_url: None,
            trust_proxy: false,
            trusted_proxies: Vec::new(),
            trust_proxy_tls: false,
            max_upload_bytes: DEFAULT_MAX_UPLOAD_BYTES,
            tls: DashboardTls::default(),
            auth: DashboardAuth::default(),
        }
    }
}

/// `[dashboard.tls]`: PEM files.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct DashboardTls {
    /// The certificate chain.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cert: Option<PathBuf>,
    /// The private key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<PathBuf>,
}

impl DashboardTls {
    /// Both files are configured.
    #[must_use]
    pub fn configured(&self) -> bool {
        self.cert.is_some() && self.key.is_some()
    }
}

/// How a remote browser authenticates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    /// Only the one-time login links `prometheus dashboard` prints
    /// (loopback binds).
    #[default]
    None,
    /// A password whose argon2id hash is the launch profile's
    /// `DASHBOARD_PASSWORD_HASH` secret.
    Password,
    /// An OpenID Connect provider.
    Oidc,
    /// The hosted account portal.
    Portal,
    /// One deployment credential.
    Token,
}

impl AuthMode {
    /// The config spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Password => "password",
            Self::Oidc => "oidc",
            Self::Portal => "portal",
            Self::Token => "token",
        }
    }
}

/// `[dashboard.auth]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct DashboardAuth {
    /// The sign-in provider for remote browsers.
    pub mode: AuthMode,
    /// Subjects that attach to sessions as drivers; every other remotely
    /// signed-in user is an observer (`password` is the password mode's
    /// subject).
    pub drivers: Vec<String>,
    /// `[dashboard.auth.oidc]`.
    pub oidc: OidcConfig,
    /// `[dashboard.auth.portal]`.
    pub portal: PortalConfig,
}

/// `[dashboard.auth.oidc]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct OidcConfig {
    /// The issuer URL (its `/.well-known/openid-configuration` is read).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issuer: Option<String>,
    /// The client id registered at the issuer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// Subjects (`sub` claims) allowed to sign in.
    pub allowed_subjects: Vec<String>,
}

/// `[dashboard.auth.portal]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct PortalConfig {
    /// The dashboard's registered OAuth client at the account portal
    /// (`prometheus dashboard register` writes it).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// The portal's OpenID issuer URL (default: the hosted account portal).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issuer: Option<String>,
}

impl PortalConfig {
    /// The issuer URL, without a trailing slash.
    #[must_use]
    pub fn issuer_url(&self) -> &str {
        self.issuer
            .as_deref()
            .unwrap_or(DEFAULT_PORTAL_ISSUER)
            .trim_end_matches('/')
    }
}

/// `daemon.toml [webapp]` (spec: web-dashboard, Desktop workspace in the
/// browser): the desktop workspace served by the dashboard listener under
/// `/app`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct WebappConfig {
    /// Serve the workspace under `/app`.
    pub enabled: bool,
    /// The address `prometheus webapp` opens, also accepted as `Host`
    /// (empty: the dashboard's own address).
    pub public_url: String,
}

/// An `http(s)` URL with no path beyond `/`: the origin a public address
/// names. Plain `http` is accepted only for loopback hosts.
///
/// # Errors
///
/// What is wrong with `url`, prefixed with `key`.
pub fn check_public_url(key: &str, url: &str) -> Result<(), String> {
    let bad = |why: &str| Err(format!("{key}: '{url}' {why}"));
    let Some((scheme, rest)) = url.split_once("://") else {
        return bad("is not an http(s) URL");
    };
    let host = rest.split('/').next().unwrap_or_default();
    let name = host.rsplit_once(':').map_or(host, |(name, port)| {
        if port.chars().all(|c| c.is_ascii_digit()) {
            name
        } else {
            host
        }
    });
    match scheme {
        _ if host.is_empty() => bad("has no host"),
        "https" => Ok(()),
        "http" if is_loopback_host(name) => Ok(()),
        "http" => bad("must use https on a non-loopback host"),
        _ => bad("is not an http(s) URL"),
    }
}

/// Whether `host` is a loopback address: `localhost` or an IP in
/// `127.0.0.0/8` or `::1`. Private (RFC 1918), CGNAT, link-local and
/// wildcard addresses and every other name are not.
#[must_use]
pub fn is_loopback_host(host: &str) -> bool {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    bare.eq_ignore_ascii_case("localhost")
        || bare.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

impl DashboardConfig {
    /// The listener's host and port, with `--host`/`--port` overrides.
    #[must_use]
    pub fn with_overrides(&self, host: Option<&str>, port: Option<u16>) -> Self {
        let mut config = self.clone();
        if let Some(host) = host {
            host.clone_into(&mut config.host);
        }
        if let Some(port) = port {
            config.port = port;
        }
        config
    }

    /// The bind is loopback.
    #[must_use]
    pub fn loopback(&self) -> bool {
        is_loopback_host(&self.host)
    }

    /// The listener serves TLS itself or behind a trusted TLS proxy.
    #[must_use]
    pub fn has_tls(&self) -> bool {
        self.tls.configured() || self.trust_proxy_tls
    }

    /// The first configuration item a non-loopback bind lacks (`None`: it
    /// may bind, secrets aside). Loopback binds lack nothing.
    #[must_use]
    pub fn missing_for_remote(&self) -> Option<String> {
        if self.loopback() {
            return None;
        }
        let oidc = &self.auth.oidc;
        let auth = match self.auth.mode {
            AuthMode::None => Some("dashboard.auth.mode".to_owned()),
            AuthMode::Oidc if oidc.issuer.is_none() => Some("dashboard.auth.oidc.issuer".into()),
            AuthMode::Oidc if oidc.client_id.is_none() => {
                Some("dashboard.auth.oidc.client_id".into())
            }
            AuthMode::Oidc if oidc.allowed_subjects.is_empty() => {
                Some("dashboard.auth.oidc.allowed_subjects".into())
            }
            AuthMode::Portal if self.auth.portal.client_id.is_none() => {
                Some("dashboard.auth.portal.client_id".into())
            }
            _ => None,
        };
        auth.or_else(|| {
            (!self.has_tls()).then(|| {
                "TLS (dashboard.tls.cert and dashboard.tls.key, or dashboard.trust_proxy_tls = true)"
                    .to_owned()
            })
        })
    }

    /// The refusal of a non-loopback bind lacking `missing`.
    #[must_use]
    pub fn refusal(&self, missing: &str) -> String {
        format!(
            "Refusing to bind dashboard to {}: {missing}. There is no unauthenticated remote dashboard.",
            self.host
        )
    }

    /// The base path without a trailing slash (`""` for `/`).
    #[must_use]
    pub fn base_prefix(&self) -> &str {
        self.base_path.trim_end_matches('/')
    }

    /// Checks every key.
    ///
    /// # Errors
    ///
    /// The validation message of the first bad key.
    pub fn validate(&self) -> Result<(), String> {
        if self.host.trim().is_empty() {
            return Err("dashboard.host: must not be empty".to_owned());
        }
        let base = &self.base_path;
        let segments_ok = base.trim_matches('/').split('/').all(|s| {
            s.is_empty()
                || s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_.~".contains(c))
        });
        if !base.starts_with('/') || base.contains("//") || !segments_ok {
            return Err(format!(
                "dashboard.base_path: '{base}' is not a path such as / or /prometheus"
            ));
        }
        if let Some(bad) = self
            .trusted_proxies
            .iter()
            .find(|p| p.parse::<IpAddr>().is_err())
        {
            return Err(format!(
                "dashboard.trusted_proxies: '{bad}' is not an IP address"
            ));
        }
        if self.max_upload_bytes == 0 {
            return Err("dashboard.max_upload_bytes: must be at least 1".to_owned());
        }
        if self.tls.cert.is_some() != self.tls.key.is_some() {
            return Err("dashboard.tls: set both cert and key".to_owned());
        }
        if let Some(url) = &self.public_url {
            check_public_url("dashboard.public_url", url)?;
        }
        if let Some(issuer) = &self.auth.portal.issuer {
            check_public_url("dashboard.auth.portal.issuer", issuer)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{AuthMode, DashboardConfig, is_loopback_host};

    #[test]
    fn the_defaults_bind_loopback_port_9120() {
        let config = DashboardConfig::default();
        assert_eq!((config.host.as_str(), config.port), ("127.0.0.1", 9120));
        assert!(config.loopback());
        assert_eq!(config.missing_for_remote(), None);
        assert_eq!(config.base_prefix(), "");
    }

    #[test]
    fn private_cgnat_link_local_and_wildcard_hosts_are_not_loopback() {
        for host in [
            "0.0.0.0",
            "192.168.1.4",
            "10.0.0.1",
            "100.64.0.1",
            "169.254.1.1",
            "::",
            "box.lan",
        ] {
            assert!(!is_loopback_host(host), "{host}");
        }
        for host in ["127.0.0.1", "127.1.2.3", "::1", "[::1]", "localhost"] {
            assert!(is_loopback_host(host), "{host}");
        }
    }

    /// Scenario: LAN bind without auth (the missing item named).
    #[test]
    fn a_remote_bind_names_the_first_missing_item() {
        let lan = DashboardConfig::default().with_overrides(Some("0.0.0.0"), None);
        assert_eq!(
            lan.missing_for_remote().as_deref(),
            Some("dashboard.auth.mode")
        );
        assert_eq!(
            lan.refusal("dashboard.auth.mode"),
            "Refusing to bind dashboard to 0.0.0.0: dashboard.auth.mode. There is no unauthenticated remote dashboard."
        );
        let mut oidc = lan.clone();
        oidc.auth.mode = AuthMode::Oidc;
        oidc.auth.oidc.issuer = Some("https://id.example".into());
        assert_eq!(
            oidc.missing_for_remote().as_deref(),
            Some("dashboard.auth.oidc.client_id")
        );
        let mut password = lan;
        password.auth.mode = AuthMode::Password;
        assert!(password.missing_for_remote().unwrap().starts_with("TLS"));
        password.trust_proxy_tls = true;
        assert_eq!(password.missing_for_remote(), None);
    }

    #[test]
    fn malformed_keys_are_refused() {
        for base in ["prometheus", "/a//b", "/a b"] {
            let config = DashboardConfig {
                base_path: base.into(),
                ..DashboardConfig::default()
            };
            assert!(config.validate().is_err(), "{base}");
        }
        let proxies = DashboardConfig {
            trusted_proxies: vec!["proxy.lan".into()],
            ..DashboardConfig::default()
        };
        assert!(proxies.validate().is_err());
        let ok = DashboardConfig {
            base_path: "/prometheus/".into(),
            ..DashboardConfig::default()
        };
        assert_eq!(ok.validate(), Ok(()));
        assert_eq!(ok.base_prefix(), "/prometheus");
    }
}

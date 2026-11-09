//! Adapter proxies and environment trust (spec: platform-adapters, Adapter
//! proxy settings and environment trust): an adapter's HTTP and WebSocket
//! traffic goes through `gateway.platforms.<name>.proxy_url` when set,
//! else `gateway.proxy_url`, else nowhere. Inherited proxy environment
//! variables and the OS proxy settings count only with
//! `gateway.trust_env = true` (`proxy_env`); otherwise a daemon started
//! with `HTTPS_PROXY` set still connects directly. A proxy
//! URL with credentials comes from the secrets store
//! (`proxy_url_secret`).

use pmt_config::schema::{GatewayConfig, PlatformConfig};
use pmt_core::error::{Error, codes};

use super::adapter::{AdapterError, AdapterSetup};
use super::proxy_env::ProxySettings;

/// How an adapter reaches the network.
#[derive(Clone, PartialEq, Eq)]
pub enum Proxy {
    /// Directly; ambient proxy settings are ignored.
    Direct,
    /// Through the proxy the environment names (`gateway.trust_env`).
    Environment,
    /// Through this proxy (`http://`, `https://` or `socks5://`).
    Url(String),
}

impl std::fmt::Debug for Proxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // A proxy URL may carry credentials.
        match self {
            Self::Direct => f.write_str("Direct"),
            Self::Environment => f.write_str("Environment"),
            Self::Url(_) => f.write_str("Url(..)"),
        }
    }
}

impl Proxy {
    /// The proxy of `platform`'s table under `gateway`, reading a
    /// `proxy_url_secret` with `secret`.
    ///
    /// # Errors
    ///
    /// The named secret is missing or unreadable.
    pub fn resolve(
        platform: &PlatformConfig,
        gateway: &GatewayConfig,
        secret: &dyn Fn(&str) -> Result<String, AdapterError>,
    ) -> Result<Self, AdapterError> {
        if let Some(name) = &platform.proxy_url_secret {
            return secret(&name.0).map(Self::Url);
        }
        if let Some(url) = &platform.proxy_url {
            return Ok(Self::Url(url.clone()));
        }
        if let Some(name) = &gateway.proxy_url_secret {
            return secret(&name.0).map(Self::Url);
        }
        if let Some(url) = &gateway.proxy_url {
            return Ok(Self::Url(url.clone()));
        }
        Ok(if gateway.trust_env {
            Self::Environment
        } else {
            Self::Direct
        })
    }

    /// The proxy of the adapter `setup` describes.
    ///
    /// # Errors
    ///
    /// As [`resolve`](Self::resolve).
    pub fn of(setup: &AdapterSetup) -> Result<Self, AdapterError> {
        let secret = |name: &str| super::adapter::credential(&setup.scope, name);
        Self::resolve(&setup.config, &setup.scope.config().gateway, &secret)
    }

    /// The proxy a WebSocket to `url` (`wss://` or `ws://`) goes
    /// through: the configured one, or under environment trust the one
    /// the environment variables or the OS settings name for it.
    #[must_use]
    pub fn for_websocket(&self, url: &str) -> Option<String> {
        match self {
            Self::Direct => None,
            Self::Url(url) => Some(url.clone()),
            Self::Environment => websocket_proxy(&ProxySettings::trusted(), url),
        }
    }
}

/// The proxy of `settings` for a WebSocket to `url`, unless its host
/// bypasses them.
fn websocket_proxy(settings: &ProxySettings, url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://").unwrap_or(("wss", url));
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = match authority.rsplit_once(':') {
        Some((host, port)) if port.parse::<u16>().is_ok() => host,
        _ => authority,
    };
    if settings.bypasses(host) {
        return None;
    }
    settings.proxy_for(matches!(scheme, "wss" | "https"))
}

/// An HTTP client builder for the adapter `setup` describes: the profile's
/// TLS roots and IPv4 rule, and the adapter's proxy (direct unless one is
/// configured or the environment is trusted).
///
/// # Errors
///
/// `net.client_init` / `net.extra_ca`; a proxy secret that cannot be read.
pub fn client_builder(setup: &AdapterSetup) -> Result<(reqwest::ClientBuilder, Proxy), Error> {
    let proxy = Proxy::of(setup).map_err(|e| Error::new(&codes::NET_CLIENT_INIT, e.message))?;
    let network = &setup.scope.config().network;
    let policy = pmt_net::OutboundPolicy {
        force_ipv4: network.force_ipv4,
        extra_ca_file: network.extra_ca_file.as_deref().map(Into::into),
        proxy: match &proxy {
            Proxy::Url(url) => Some(url.clone()),
            Proxy::Direct | Proxy::Environment => None,
        },
    };
    let builder = pmt_net::http::client_builder(&policy)?;
    let builder = match &proxy {
        // An explicit proxy already replaces the environment's.
        Proxy::Url(_) => builder,
        Proxy::Direct => builder.no_proxy(),
        Proxy::Environment => with_settings(builder, &ProxySettings::trusted())?,
    };
    Ok((builder, proxy))
}

/// `builder` going through the proxies of `settings` (and directly when
/// they name none): reqwest itself reads only the environment variables,
/// so the merged settings replace its own lookup.
fn with_settings(
    builder: reqwest::ClientBuilder,
    settings: &ProxySettings,
) -> Result<reqwest::ClientBuilder, Error> {
    let no_proxy = reqwest::NoProxy::from_string(&settings.no_proxy());
    let invalid = |url: &str, e: reqwest::Error| {
        Error::new(
            &codes::NET_CLIENT_INIT,
            format!("the environment's proxy {url} is not a proxy URL: {e}"),
        )
    };
    let mut builder = builder.no_proxy();
    if let Some(url) = settings.proxy_for(false) {
        let proxy = reqwest::Proxy::http(&url).map_err(|e| invalid(&url, e))?;
        builder = builder.proxy(proxy.no_proxy(no_proxy.clone()));
    }
    if let Some(url) = settings.proxy_for(true) {
        let proxy = reqwest::Proxy::https(&url).map_err(|e| invalid(&url, e))?;
        builder = builder.proxy(proxy.no_proxy(no_proxy));
    }
    Ok(builder)
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::*;

    fn no_secret(name: &str) -> Result<String, AdapterError> {
        Err(AdapterError {
            retryable: false,
            code: "missing_secret".into(),
            message: name.into(),
        })
    }

    #[test]
    fn the_platform_proxy_wins_then_the_shared_one_then_direct() {
        let gateway = GatewayConfig {
            proxy_url: Some("socks5://shared:1080".into()),
            ..GatewayConfig::default()
        };
        let platform = PlatformConfig {
            proxy_url: Some("http://own:3128".into()),
            ..PlatformConfig::default()
        };
        assert_eq!(
            Proxy::resolve(&platform, &gateway, &no_secret).unwrap(),
            Proxy::Url("http://own:3128".into())
        );
        assert_eq!(
            Proxy::resolve(&PlatformConfig::default(), &gateway, &no_secret).unwrap(),
            Proxy::Url("socks5://shared:1080".into())
        );
        let secret = |_: &str| Ok("http://u:pw@vault:3128".to_owned());
        let platform = PlatformConfig {
            proxy_url_secret: Some(pmt_config::schema::SecretName("PROXY".into())),
            ..PlatformConfig::default()
        };
        assert_eq!(
            Proxy::resolve(&platform, &GatewayConfig::default(), &secret).unwrap(),
            Proxy::Url("http://u:pw@vault:3128".into())
        );
    }

    #[test]
    fn ambient_proxy_settings_count_only_with_trust_env() {
        let direct = Proxy::resolve(
            &PlatformConfig::default(),
            &GatewayConfig::default(),
            &no_secret,
        )
        .unwrap();
        assert_eq!(direct, Proxy::Direct);
        // Whatever the environment holds, a direct adapter tunnels nowhere.
        assert_eq!(direct.for_websocket("wss://gateway.example"), None);
        let trusting = GatewayConfig {
            trust_env: true,
            ..GatewayConfig::default()
        };
        assert_eq!(
            Proxy::resolve(&PlatformConfig::default(), &trusting, &no_secret).unwrap(),
            Proxy::Environment
        );
    }

    /// macOS-style settings naming `proxy` for HTTP, bypassing `bypass`.
    fn os_settings(proxy: std::net::SocketAddr, bypass: &str) -> ProxySettings {
        ProxySettings::from_scutil(&format!(
            "<dictionary> {{\n  ExceptionsList : <array> {{\n    0 : {bypass}\n  }}\n  \
             HTTPEnable : 1\n  HTTPPort : {}\n  HTTPProxy : {}\n}}\n",
            proxy.port(),
            proxy.ip()
        ))
    }

    #[tokio::test]
    async fn os_proxy_settings_carry_requests_under_environment_trust() {
        let (proxy, log) = crate::messaging::mock_server::proxy().await.unwrap();
        let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = target.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = target.accept().await {
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf).await;
                let _ = socket
                    .write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
                    )
                    .await;
            }
        });
        let get = |settings: ProxySettings| async move {
            let base = pmt_net::http::client_builder(&pmt_net::OutboundPolicy::default()).unwrap();
            let client = with_settings(base, &settings).unwrap().build().unwrap();
            client
                .get(format!("http://{addr}/hello"))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap()
        };
        assert_eq!(get(os_settings(proxy, "*.local")).await, "ok");
        assert_eq!(
            log.lock().unwrap().clone(),
            vec![format!("GET http://{addr}/hello")],
            "the OS proxy carried the request"
        );
        assert_eq!(get(os_settings(proxy, "127.0.0.1")).await, "ok");
        assert_eq!(log.lock().unwrap().len(), 1, "an excepted host is direct");
        assert_eq!(get(ProxySettings::default()).await, "ok");
        assert_eq!(log.lock().unwrap().len(), 1, "no settings, no proxy");
    }

    #[test]
    fn websockets_take_the_environments_proxy_for_their_scheme_and_host() {
        let settings = ProxySettings {
            https: Some("http://secure:3129".into()),
            all: Some("socks5://sock:1080".into()),
            bypass: vec![".internal.example".into()],
            ..ProxySettings::default()
        };
        assert_eq!(
            websocket_proxy(&settings, "wss://gateway.discord.gg/?v=10").as_deref(),
            Some("http://secure:3129")
        );
        assert_eq!(
            websocket_proxy(&settings, "ws://chat.example:8080/socket").as_deref(),
            Some("socks5://sock:1080")
        );
        assert_eq!(
            websocket_proxy(&settings, "wss://bus.internal.example:443/"),
            None
        );
    }
}

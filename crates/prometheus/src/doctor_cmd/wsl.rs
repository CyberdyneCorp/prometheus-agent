//! The WSL2 check of the `providers` group (spec: model-providers, WSL2
//! local endpoint diagnostics): a local provider's loopback endpoint that
//! does not answer under WSL2 NAT networking is a WARN naming mirrored
//! networking and the Windows host address, never a failure.

use std::time::Duration;

use pmt_core::doctor::{Check, Status};
use pmt_providers::wsl::{Wsl, advice};

/// How long a loopback probe may take.
const PROBE: Duration = Duration::from_millis(500);

/// The local providers' loopback endpoints (`(provider, base_url)`) that
/// `reachable` says do not answer, as WARN checks under `wsl`.
pub(super) async fn checks<F, Fut>(
    wsl: Option<Wsl>,
    endpoints: &[(String, String)],
    reachable: F,
) -> Vec<Check>
where
    F: Fn(String) -> Fut,
    Fut: Future<Output = bool>,
{
    let Some(wsl) = wsl else {
        return Vec::new();
    };
    let mut checks = Vec::new();
    for (provider, base_url) in endpoints {
        let Some(text) = advice(&wsl, base_url) else {
            continue;
        };
        if reachable(base_url.clone()).await {
            continue;
        }
        checks.push(
            Check::new(
                "providers",
                "wsl2_loopback",
                Status::Warn,
                format!("{provider}: {base_url} does not answer from WSL2"),
            )
            .with_hint(text),
        );
    }
    checks
}

/// Whether `base_url`'s host and port accept a TCP connection.
pub(super) async fn answers(base_url: String) -> bool {
    let Ok(url) = reqwest::Url::parse(&base_url) else {
        return false;
    };
    let (Some(host), Some(port)) = (url.host_str(), url.port_or_known_default()) else {
        return false;
    };
    let host = host.trim_matches(['[', ']']).to_owned();
    matches!(
        tokio::time::timeout(PROBE, tokio::net::TcpStream::connect((host.as_str(), port))).await,
        Ok(Ok(_))
    )
}

/// The base URLs of the profile's local providers (the configured main
/// provider and fallback entries).
pub(super) fn local_endpoints(scope: &pmt_config::ProfileScope) -> Vec<(String, String)> {
    let config = scope.config();
    let Ok(registry) = pmt_providers::ProviderRegistry::for_profile(config, Vec::new()) else {
        return Vec::new();
    };
    let ids = config
        .model
        .provider
        .iter()
        .chain(config.fallback.chain.iter().map(|e| &e.provider));
    let mut found: Vec<(String, String)> = Vec::new();
    for id in ids {
        let Some(provider) = registry.lookup(id).ok().filter(|p| p.descriptor().local) else {
            continue;
        };
        if let Some(url) = provider.base_url() {
            let entry = (provider.id().to_owned(), url.to_owned());
            if !found.contains(&entry) {
                found.push(entry);
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;

    /// Scenario: Windows-hosted Ollama unreachable from WSL2.
    #[tokio::test]
    async fn an_unreachable_loopback_server_under_wsl2_nat_is_a_warn_with_the_host_ip() {
        let wsl = Wsl {
            nat: true,
            host_ip: Some(Ipv4Addr::new(172, 29, 64, 1)),
        };
        let endpoints = vec![("ollama".to_owned(), "http://127.0.0.1:11434/v1".to_owned())];
        let checks = checks(Some(wsl), &endpoints, |_| async { false }).await;
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, Status::Warn);
        let hint = checks[0].hint.as_deref().unwrap();
        assert!(hint.contains("mirrored networking"), "{hint}");
        assert!(hint.contains("172.29.64.1"), "{hint}");

        assert!(checks_for(Some(wsl), &endpoints, true).await.is_empty());
        assert!(checks_for(None, &endpoints, false).await.is_empty());
    }

    async fn checks_for(wsl: Option<Wsl>, endpoints: &[(String, String)], up: bool) -> Vec<Check> {
        checks(wsl, endpoints, move |_| async move { up }).await
    }
}

//! Transport check of configured provider base URLs (spec: model-providers,
//! Provider descriptor contract): a provider's key and prompts may travel
//! over plain HTTP only to loopback or private-network hosts, unless the
//! provider opts in with `allow_insecure_http = true`.

use std::net::IpAddr;

use pmt_core::error::{Error, codes};

use crate::schema::{Config, ModelAlias, ModelEntry};

/// Why a base URL is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UrlProblem {
    /// Not an `http://` or `https://` URL with a host.
    Malformed,
    /// Plain HTTP to a host that is neither loopback nor private.
    InsecurePublic {
        /// The host.
        host: String,
    },
}

/// The host of an `http(s)://` URL, lower-cased, without brackets or port.
#[must_use]
pub fn host_of(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = if let Some(bracketed) = authority.strip_prefix('[') {
        bracketed.split(']').next()?
    } else {
        authority.split(':').next()?
    };
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// Whether `host` is loopback or a private-network address (`localhost`,
/// `*.localhost`, RFC 1918, link-local, IPv6 unique-local).
#[must_use]
pub fn is_local_host(host: &str) -> bool {
    if host == "localhost" || host.ends_with(".localhost") {
        return true;
    }
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => ip.is_loopback() || ip.is_private() || ip.is_link_local(),
        Ok(IpAddr::V6(ip)) => {
            ip.is_loopback() || ip.is_unique_local() || ip.is_unicast_link_local()
        }
        Err(_) => false,
    }
}

/// Checks one base URL.
///
/// # Errors
///
/// The [`UrlProblem`].
pub fn check_url(url: &str, allow_insecure_http: bool) -> Result<(), UrlProblem> {
    let host = host_of(url).ok_or(UrlProblem::Malformed)?;
    if url.starts_with("http://") && !allow_insecure_http && !is_local_host(&host) {
        return Err(UrlProblem::InsecurePublic { host });
    }
    Ok(())
}

/// The `config.invalid` error for `key` (whose provider is `provider`).
#[must_use]
pub fn problem_error(key: &str, provider: &str, url: &str, problem: &UrlProblem) -> Error {
    match problem {
        UrlProblem::Malformed => Error::new(
            &codes::CONFIG_INVALID,
            format!("{key}: '{url}' is not an http:// or https:// URL"),
        ),
        UrlProblem::InsecurePublic { host } => Error::new(
            &codes::CONFIG_INVALID,
            format!(
                "{key}: plain HTTP to public host '{host}' is refused; use https:// or set providers.{provider}.allow_insecure_http = true"
            ),
        )
        .with_hint(format!(
            "prometheus config set providers.{provider}.allow_insecure_http true"
        )),
    }
}

/// Checks every configured base URL: `providers.*`, `[[fallback.chain]]`
/// entries and `[models]` routes (the latter two use their provider's
/// `allow_insecure_http`).
///
/// # Errors
///
/// `config.invalid` (exit 78) naming the first offending key and the
/// `allow_insecure_http` remedy.
pub fn check(config: &Config) -> Result<(), Error> {
    let allows = |provider: &str| {
        config
            .providers
            .get(provider)
            .is_some_and(|p| p.allow_insecure_http)
    };
    let mut urls: Vec<(String, String, &str)> = Vec::new();
    for (id, provider) in &config.providers {
        if let Some(url) = &provider.base_url {
            urls.push((format!("providers.{id}.base_url"), id.clone(), url));
        }
    }
    for (i, entry) in config.fallback.chain.iter().enumerate() {
        if let Some(url) = &entry.base_url {
            let key = format!("fallback.chain[{i}].base_url");
            urls.push((key, entry.provider.clone(), url));
        }
    }
    for (name, entry) in &config.models {
        if let ModelEntry::Route(route) = entry
            && let (Some(url), Some(provider)) = (&route.base_url, &route.provider)
        {
            urls.push((format!("models.{name}.base_url"), provider.clone(), url));
        }
    }
    for (name, alias) in &config.model.aliases {
        if let ModelAlias::Route(route) = alias
            && let Some(url) = &route.base_url
        {
            let key = format!("model.aliases.{name}.base_url");
            urls.push((key, route.provider.clone(), url));
        }
    }
    for (key, provider, url) in urls {
        check_url(url, allows(&provider))
            .map_err(|problem| problem_error(&key, &provider, url, &problem))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(text: &str) -> Config {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn plain_http_to_a_public_host_is_refused_naming_the_key_and_remedy() {
        let err = check(&config(
            "[providers.myproxy]\nbase_url = \"http://llm.example.com/v1\"\n",
        ))
        .unwrap_err();
        assert_eq!(err.exit_code(), 78);
        assert!(err.message().starts_with("providers.myproxy.base_url:"));
        assert!(err.message().contains("allow_insecure_http"));
    }

    #[test]
    fn local_http_and_opt_in_are_accepted() {
        for url in [
            "http://127.0.0.1:11434/v1",
            "http://localhost:8000/v1",
            "http://192.168.1.20/v1",
            "http://[::1]:8080/v1",
            "https://llm.example.com/v1",
        ] {
            let text = format!("[providers.x]\nbase_url = \"{url}\"\n");
            assert!(check(&config(&text)).is_ok(), "{url}");
        }
        let text =
            "[providers.x]\nbase_url = \"http://llm.example.com\"\nallow_insecure_http = true\n";
        assert!(check(&config(text)).is_ok());
    }

    #[test]
    fn fallback_entries_are_checked_too() {
        let text = "[[fallback.chain]]\nprovider = \"custom\"\nmodel = \"m\"\nbase_url = \"http://203.0.113.9/v1\"\n";
        let err = check(&config(text)).unwrap_err();
        assert!(err.message().starts_with("fallback.chain[0].base_url:"));
    }

    #[test]
    fn hosts_are_extracted_without_port_userinfo_or_brackets() {
        assert_eq!(
            host_of("https://API.x.com:443/v1").as_deref(),
            Some("api.x.com")
        );
        assert_eq!(host_of("http://u:p@h.test/x").as_deref(), Some("h.test"));
        assert_eq!(host_of("http://[::1]:1/").as_deref(), Some("::1"));
        assert_eq!(host_of("ftp://x"), None);
    }
}

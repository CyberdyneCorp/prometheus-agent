//! Which backend answers each web capability (spec: web-and-media-tools,
//! Web backend selection precedence).
//!
//! `web.search_backend` / `web.extract_backend` win over the shared
//! `web.backend`, which wins over `auto`. `auto` walks a fixed order and
//! takes the first backend whose credentials are present; `xai` and
//! `openai-native` are never in that order, so a stray xAI key never
//! reroutes web search.

use pmt_config::schema::WebConfig;

use super::Capability;
use super::backend::FIRECRAWL_SECRET;

/// Backend ids this build implements.
pub const BACKENDS: &[&str] = &["firecrawl", "searxng"];

/// The `auto` order of the spec. Entries this build does not implement
/// are skipped (they arrive with the M2 backend list).
pub const AUTO_ORDER: &[&str] = &[
    "tavily",
    "perplexity",
    "exa",
    "parallel",
    "firecrawl",
    "nous",
    "searxng",
    "brave-free",
    "keyless",
];

/// Backends that only search.
const SEARCH_ONLY: &[&str] = &["searxng", "brave-free", "xai", "openai-native"];

/// The reason the web tools give when nothing resolves.
pub const NOTHING_CONFIGURED: &str = "no web backend configured";

/// Answers whether a secret is present in the profile's secrets store.
pub type HasSecret<'a> = &'a (dyn Fn(&str) -> bool + Send + Sync);

/// The backend id serving `capability`, or why there is none (shown as
/// `unavailable:<reason>`).
///
/// # Errors
///
/// [`NOTHING_CONFIGURED`] when `auto` finds no credentials, a search-only
/// backend chosen for extract, an id this build does not implement, or a
/// backend missing its required settings.
pub fn resolve(
    config: &WebConfig,
    capability: Capability,
    has_secret: HasSecret<'_>,
) -> Result<&'static str, String> {
    let specific = match capability {
        Capability::Search => config.search_backend.as_deref(),
        Capability::Extract => config.extract_backend.as_deref(),
    };
    let id = specific.unwrap_or(&config.backend);
    if id == "auto" {
        return auto(config, capability, has_secret).ok_or_else(|| NOTHING_CONFIGURED.to_owned());
    }
    if capability == Capability::Extract && SEARCH_ONLY.contains(&id) {
        return Err(format!(
            "backend '{id}' is search-only; set web.extract_backend"
        ));
    }
    let Some(known) = BACKENDS.iter().copied().find(|known| *known == id) else {
        return Err(format!("web backend '{id}' is not available in this build"));
    };
    if known == "searxng" && config.searxng.url.is_none() {
        return Err("backend 'searxng' needs web.searxng.url".to_owned());
    }
    Ok(known)
}

/// The first backend of [`AUTO_ORDER`] this build implements, that serves
/// `capability` and whose credentials are present.
fn auto(
    config: &WebConfig,
    capability: Capability,
    has_secret: HasSecret<'_>,
) -> Option<&'static str> {
    AUTO_ORDER.iter().copied().find(|id| {
        BACKENDS.contains(id)
            && !(capability == Capability::Extract && SEARCH_ONLY.contains(id))
            && credentials_present(id, config, has_secret)
    })
}

/// Whether `id` can be called without further setup.
fn credentials_present(id: &str, config: &WebConfig, has_secret: HasSecret<'_>) -> bool {
    match id {
        "firecrawl" => config.firecrawl.base_url.is_some() || has_secret(FIRECRAWL_SECRET),
        "searxng" => config.searxng.url.is_some(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use pmt_config::schema::WebConfig;

    use super::{Capability, NOTHING_CONFIGURED, resolve};

    fn none(_: &str) -> bool {
        false
    }

    #[test]
    fn per_capability_keys_win_over_the_shared_backend() {
        let mut config = WebConfig {
            backend: "firecrawl".into(),
            search_backend: Some("searxng".into()),
            ..WebConfig::default()
        };
        config.searxng.url = Some("http://127.0.0.1:8888".into());
        assert_eq!(resolve(&config, Capability::Search, &none), Ok("searxng"));
        assert_eq!(
            resolve(&config, Capability::Extract, &none),
            Ok("firecrawl")
        );
        config.extract_backend = Some("searxng".into());
        assert_eq!(
            resolve(&config, Capability::Extract, &none),
            Err("backend 'searxng' is search-only; set web.extract_backend".into())
        );
        config.search_backend = Some("tavily".into());
        assert!(
            resolve(&config, Capability::Search, &none)
                .unwrap_err()
                .contains("tavily")
        );
    }

    #[test]
    fn an_xai_key_does_not_reroute_web_search() {
        let config = WebConfig::default();
        let only_xai = |name: &str| name == "XAI_API_KEY";
        for capability in [Capability::Search, Capability::Extract] {
            assert_eq!(
                resolve(&config, capability, &only_xai),
                Err(NOTHING_CONFIGURED.into())
            );
        }
    }

    #[test]
    fn auto_takes_the_first_backend_with_credentials_per_capability() {
        let mut config = WebConfig::default();
        config.searxng.url = Some("http://127.0.0.1:8888".into());
        // searxng is configured but search-only: extract stays unavailable.
        assert_eq!(resolve(&config, Capability::Search, &none), Ok("searxng"));
        assert_eq!(
            resolve(&config, Capability::Extract, &none),
            Err(NOTHING_CONFIGURED.into())
        );
        // A Firecrawl key comes earlier in the order for both.
        let firecrawl = |name: &str| name == "FIRECRAWL_API_KEY";
        assert_eq!(
            resolve(&config, Capability::Search, &firecrawl),
            Ok("firecrawl")
        );
        assert_eq!(
            resolve(&config, Capability::Extract, &firecrawl),
            Ok("firecrawl")
        );
        // Keyless is reached only when enabled, and is not built in M1.
        config.searxng.url = None;
        config.keyless.enabled = true;
        assert_eq!(
            resolve(&config, Capability::Search, &none),
            Err(NOTHING_CONFIGURED.into())
        );
    }
}

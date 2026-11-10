//! Which backend answers each web capability (spec: web-and-media-tools,
//! Web backend selection precedence).
//!
//! `web.search_backend` / `web.extract_backend` win over the shared
//! `web.backend`, which wins over `auto`. `auto` walks a fixed order and
//! takes the first backend whose credentials are present; `xai` and
//! `openai-native` are never in that order, so a stray xAI key never
//! reroutes web search. A sign-in is asked of the same presence check
//! as `signin:<provider>` ([`crate::probe::signin`]).

use pmt_config::schema::WebConfig;

use super::Capability;
use super::backend::FIRECRAWL_SECRET;

/// Backend ids this build implements.
pub const BACKENDS: &[&str] = &[
    "firecrawl",
    "searxng",
    "tavily",
    "exa",
    "parallel",
    "keenable",
    "perplexity",
    "brave-free",
    "ddgs",
    "xai",
    "openai-native",
    "nous",
    "keyless",
];

/// The `auto` order of the spec (`xai` and `openai-native` never appear,
/// `keyless` only when `web.keyless.enabled`).
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
pub const SEARCH_ONLY: &[&str] = &["searxng", "brave-free", "ddgs", "xai", "openai-native"];

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
    let id = configured(config, capability);
    if id == "auto" {
        return auto(config, capability, has_secret).ok_or_else(|| NOTHING_CONFIGURED.to_owned());
    }
    if capability == Capability::Extract && SEARCH_ONLY.contains(&id) {
        return Err(format!(
            "backend '{id}' is search-only; set web.extract_backend"
        ));
    }
    let Some(known) = BACKENDS.iter().copied().find(|known| *known == id) else {
        // Spec: plugin-system, Provider-object plugins fail closed when
        // unregistered: never another backend in its place.
        return Err(not_installed(capability, id));
    };
    // An explicit choice is honoured: a missing key fails the call naming
    // it, never another backend. Only a missing instance URL is fatal.
    if known == "searxng" && config.searxng.url.is_none() {
        return Err("backend 'searxng' needs web.searxng.url".to_owned());
    }
    Ok(known)
}

/// The secret a keyed backend reads.
#[must_use]
pub fn secret_of(id: &str) -> Option<&'static str> {
    if id == "firecrawl" {
        return Some(FIRECRAWL_SECRET);
    }
    super::vendors::KEYED
        .iter()
        .find(|spec| spec.id == id)
        .and_then(|spec| spec.secret)
}

/// The error of a backend name nothing provides (neither this build nor
/// an enabled plugin).
#[must_use]
pub fn not_installed(capability: Capability, id: &str) -> String {
    let category = match capability {
        Capability::Search => "web_search",
        Capability::Extract => "web_extract",
    };
    format!("{category} provider '{id}' is not installed or not enabled")
}

/// The id configured for `capability` (`auto` included).
#[must_use]
pub fn configured(config: &WebConfig, capability: Capability) -> &str {
    let specific = match capability {
        Capability::Search => config.search_backend.as_deref(),
        Capability::Extract => config.extract_backend.as_deref(),
    };
    specific.unwrap_or(&config.backend)
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
            && (*id != super::keyless::KEYLESS || config.keyless.enabled)
            && !(capability == Capability::Extract && SEARCH_ONLY.contains(id))
            && credentials_present(id, config, has_secret)
    })
}

/// Whether `id` can be called without further setup.
fn credentials_present(id: &str, config: &WebConfig, has_secret: HasSecret<'_>) -> bool {
    match id {
        "firecrawl" => config.firecrawl.base_url.is_some() || has_secret(FIRECRAWL_SECRET),
        "searxng" => config.searxng.url.is_some(),
        "nous" => has_secret(&crate::probe::signin("nous")),
        "xai" => has_secret("XAI_API_KEY") || has_secret(&crate::probe::signin("xai-oauth")),
        // Named explicitly, the ring is the choice; `auto` reaches it only
        // when enabled ([`auto`]).
        "keyless" | "ddgs" | "openai-native" => true,
        other => secret_of(other).is_some_and(has_secret),
    }
}

#[cfg(test)]
mod tests {
    use pmt_config::schema::WebConfig;

    use super::{Capability, FIRECRAWL_SECRET, NOTHING_CONFIGURED, resolve};

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
        // An explicit keyed backend is honoured without its key (the call
        // names the missing key).
        config.search_backend = Some("tavily".into());
        assert_eq!(resolve(&config, Capability::Search, &none), Ok("tavily"));
    }

    #[test]
    fn an_unknown_backend_is_not_installed_and_never_replaced() {
        // Spec: plugin-system, Provider-object plugins fail closed when
        // unregistered: firecrawl has credentials, but `fal` was named.
        let config = WebConfig {
            backend: "fal".into(),
            ..WebConfig::default()
        };
        let firecrawl = |name: &str| name == FIRECRAWL_SECRET;
        assert_eq!(
            resolve(&config, Capability::Search, &firecrawl),
            Err("web_search provider 'fal' is not installed or not enabled".into())
        );
        assert_eq!(
            resolve(&config, Capability::Extract, &firecrawl),
            Err("web_extract provider 'fal' is not installed or not enabled".into())
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
        // Keyless is reached only when enabled.
        config.searxng.url = None;
        assert_eq!(
            resolve(&config, Capability::Search, &none),
            Err(NOTHING_CONFIGURED.into())
        );
        config.keyless.enabled = true;
        assert_eq!(resolve(&config, Capability::Search, &none), Ok("keyless"));
        assert_eq!(resolve(&config, Capability::Extract, &none), Ok("keyless"));
    }
}

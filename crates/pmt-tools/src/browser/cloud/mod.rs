//! Cloud browser providers (spec: execution-environments, First-party
//! cloud browser providers; Browser session resolution).
//!
//! A provider creates a remote browser and returns its DevTools WebSocket
//! endpoint; the session releases it when it ends. The first-party
//! providers ship in the binary, each described by a [`Manifest`] naming
//! the secrets it reads and the hosts it talks to: `browserbase`,
//! `browser-use`, `firecrawl` and `subscription` (the provider
//! subscription's tool gateway). A provider is used only when
//! `browser.provider` names it; a provider key in the secrets store never
//! selects one ([`suggestion`] lets setup mention it).

mod http;
mod vendors;

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

pub use http::{Endpoint, HttpClient, TokenSource};
pub use vendors::{BrowserUse, Browserbase, Firecrawl, GATEWAY_ORIGIN, Subscription};

use crate::web::SecretLookup;

/// A remote browser a provider created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudSession {
    /// The provider's id of the session (released with it).
    pub id: String,
    /// Its DevTools WebSocket endpoint.
    pub endpoint: String,
}

/// What a provider's create call returns.
pub type CreateFuture<'a> = Pin<Box<dyn Future<Output = Result<CloudSession, String>> + Send + 'a>>;

/// What a provider's release call returns.
pub type ReleaseFuture<'a> = Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

/// A cloud browser provider (`browser.provider`).
pub trait CloudProvider: Send + Sync {
    /// Creates a browser session.
    fn create(&self) -> CreateFuture<'_>;

    /// Releases session `id` (a provider bills until then).
    fn release<'a>(&'a self, _id: &'a str) -> ReleaseFuture<'a> {
        Box::pin(async { Ok(()) })
    }
}

/// What a first-party provider declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Manifest {
    /// The `browser.provider` name.
    pub id: &'static str,
    /// One line for listings and setup.
    pub description: &'static str,
    /// Secrets it reads from the profile's secrets store (the first ones
    /// are required, see `required`).
    pub secrets: &'static [&'static str],
    /// How many of `secrets` it cannot work without.
    pub required: usize,
    /// Hosts it connects to (`*.` matches any subdomain): its API and its
    /// browsers' DevTools endpoints. A base URL the user sets in the
    /// secrets store is allowed too.
    pub hosts: &'static [&'static str],
}

/// The first-party providers.
pub const MANIFESTS: &[Manifest] = &[
    Manifest {
        id: "browserbase",
        description: "Browserbase cloud browsers (stealth, proxies, keep-alive)",
        secrets: &[
            "BROWSERBASE_API_KEY",
            "BROWSERBASE_PROJECT_ID",
            "BROWSERBASE_BASE_URL",
        ],
        required: 2,
        hosts: &["api.browserbase.com", "*.browserbase.com"],
    },
    Manifest {
        id: "browser-use",
        description: "Browser Use cloud browsers",
        secrets: &["BROWSER_USE_API_KEY"],
        required: 1,
        hosts: &["api.browser-use.com", "*.browser-use.com"],
    },
    Manifest {
        id: "firecrawl",
        description: "Firecrawl cloud browsers",
        secrets: &["FIRECRAWL_API_KEY", "FIRECRAWL_API_URL"],
        required: 1,
        hosts: &["api.firecrawl.dev", "*.firecrawl.dev"],
    },
    Manifest {
        id: SUBSCRIPTION,
        description: "Cloud browsers through the signed-in provider subscription's tool gateway",
        secrets: &[],
        required: 0,
        hosts: &["*.nousresearch.com", "*.browser-use.com"],
    },
];

/// The provider served by a signed-in subscription.
pub const SUBSCRIPTION: &str = "subscription";

/// The subscription provider's own name, an alias of [`SUBSCRIPTION`]
/// (spec: model-providers, Subscription tool gateway routing: a
/// category's selection names the provider id).
pub const NOUS: &str = "nous";

/// The manifest of provider `id` (`nous` is the subscription one).
#[must_use]
pub fn manifest(id: &str) -> Option<&'static Manifest> {
    let id = if id == NOUS { SUBSCRIPTION } else { id };
    MANIFESTS.iter().find(|m| m.id == id)
}

/// Every provider name `browser.provider` accepts besides `local`.
#[must_use]
pub fn names() -> Vec<&'static str> {
    MANIFESTS.iter().map(|m| m.id).collect()
}

/// A provider setup may suggest because its required secrets are present
/// (never selected by this: `browser.provider` stays as configured).
#[must_use]
pub fn suggestion(has_secret: &dyn Fn(&str) -> bool) -> Option<&'static str> {
    MANIFESTS
        .iter()
        .filter(|m| m.required > 0)
        .find(|m| m.secrets[..m.required].iter().all(|s| has_secret(s)))
        .map(|m| m.id)
}

/// The first-party providers over `client`, reading secrets with
/// `secrets` and the subscription token with `token`.
#[must_use]
pub fn first_party(
    client: &HttpClient,
    secrets: &SecretLookup,
    token: TokenSource,
) -> BTreeMap<String, Arc<dyn CloudProvider>> {
    let mut providers: BTreeMap<String, Arc<dyn CloudProvider>> = BTreeMap::new();
    providers.insert(
        "browserbase".to_owned(),
        Arc::new(Browserbase::new(Arc::clone(client), Arc::clone(secrets))),
    );
    providers.insert(
        "browser-use".to_owned(),
        Arc::new(BrowserUse::new(Arc::clone(client), Arc::clone(secrets))),
    );
    providers.insert(
        "firecrawl".to_owned(),
        Arc::new(Firecrawl::new(Arc::clone(client), Arc::clone(secrets))),
    );
    let subscription: Arc<dyn CloudProvider> =
        Arc::new(Subscription::new(Arc::clone(client), token));
    providers.insert(NOUS.to_owned(), Arc::clone(&subscription));
    providers.insert(SUBSCRIPTION.to_owned(), subscription);
    providers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_manifest_names_its_hosts_and_required_secrets() {
        for manifest in MANIFESTS {
            assert!(!manifest.hosts.is_empty(), "{}", manifest.id);
            assert!(
                manifest.required <= manifest.secrets.len(),
                "{}",
                manifest.id
            );
        }
        assert_eq!(
            names(),
            ["browserbase", "browser-use", "firecrawl", "subscription"]
        );
    }

    #[test]
    fn a_present_key_is_only_a_suggestion() {
        let has = |name: &str| name.starts_with("BROWSERBASE_");
        assert_eq!(suggestion(&has), Some("browserbase"));
        assert_eq!(suggestion(&|_: &str| false), None);
    }
}

//! The `keyless` web backend: a round-robin ring over the public free
//! tiers of Exa, Parallel, Firecrawl and Keenable (spec: web-and-media-
//! tools, Keyless web search rotation is explicit opt-in).
//!
//! Each call starts at the next vendor of the ring and moves on when a
//! vendor rate-limits or refuses it; any other failure ends the call. A
//! vendor pinned to `paid` by `web.provider_tier.<vendor>` uses its key,
//! and without the key it leaves the ring (the reason is kept for
//! `doctor`). The ring is only built when config names `keyless` or opts
//! in; nothing here runs on a default install.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use pmt_config::schema::{ProviderTier, WebConfig};

use super::backend::{Hit, Page, SecretLookup, WebBackend};
use super::vendors::{Credential, EXA, FIRECRAWL, KEENABLE, PARALLEL, Vendor, VendorSpec};

/// The ring's vendors, in rotation order.
pub const RING: [&VendorSpec; 4] = [&EXA, &PARALLEL, &FIRECRAWL, &KEENABLE];

/// The id config names the ring by.
pub const KEYLESS: &str = "keyless";

/// Whether a failure moves the call to the next vendor: a rate limit or
/// a refusal (`HTTP 401/402/403/429`).
#[must_use]
pub fn rotates(failure: &str) -> bool {
    ["HTTP 401", "HTTP 402", "HTTP 403", "HTTP 429"]
        .iter()
        .any(|status| failure.starts_with(status))
}

/// The keyless ring of a session.
pub struct Ring {
    vendors: Vec<Vendor>,
    next: AtomicUsize,
    problems: Vec<String>,
}

impl Ring {
    /// The ring per `config`'s tiers; `endpoints` overrides a vendor's base
    /// URL by id (tests, mirrors).
    #[must_use]
    pub fn new(
        config: &WebConfig,
        client: &reqwest::Client,
        secrets: &SecretLookup,
        endpoints: &BTreeMap<String, String>,
    ) -> Self {
        let mut vendors = Vec::new();
        let mut problems = Vec::new();
        for spec in RING {
            let credential = match config.provider_tier.get(spec.id) {
                Some(ProviderTier::Paid) => {
                    let secret = spec.secret.unwrap_or_default();
                    if secrets(secret).is_none() {
                        problems.push(format!(
                            "web.provider_tier.{} = \"paid\" but {secret} is not set; {} left the keyless ring",
                            spec.id, spec.id
                        ));
                        continue;
                    }
                    Credential::Key(std::sync::Arc::clone(secrets))
                }
                Some(ProviderTier::Free) | None => Credential::Anonymous,
            };
            let base = endpoints.get(spec.id).map(String::as_str);
            vendors.push(Vendor::new(spec, client.clone(), base, credential));
        }
        Self {
            vendors,
            next: AtomicUsize::new(0),
            problems,
        }
    }

    /// Why vendors left the ring.
    #[must_use]
    pub fn problems(&self) -> &[String] {
        &self.problems
    }

    /// The vendors of one call, starting at the next in rotation.
    fn order(&self) -> impl Iterator<Item = &Vendor> {
        let count = self.vendors.len();
        let start = if count == 0 {
            0
        } else {
            self.next.fetch_add(1, Ordering::Relaxed) % count
        };
        (0..count).map(move |i| &self.vendors[(start + i) % count])
    }

    fn empty(&self) -> String {
        match self.problems.first() {
            Some(problem) => format!("the keyless ring has no vendor ({problem})"),
            None => "the keyless ring has no vendor".to_owned(),
        }
    }

    /// Searches on the ring: the hits and the vendor that answered.
    ///
    /// # Errors
    ///
    /// `(vendor, reason)` of the last failure.
    pub async fn search(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<(Vec<Hit>, String), (String, String)> {
        let mut last = (KEYLESS.to_owned(), self.empty());
        for vendor in self.order() {
            match vendor.search(query, limit).await {
                Ok(hits) => return Ok((hits, vendor.id().to_owned())),
                Err(why) if rotates(&why) => last = (vendor.id().to_owned(), why),
                Err(why) => return Err((vendor.id().to_owned(), why)),
            }
        }
        Err(last)
    }

    /// Extracts on the ring: the page and the vendor that answered.
    ///
    /// # Errors
    ///
    /// `(vendor, reason)` of the last failure.
    pub async fn extract(&self, url: &str) -> Result<(Page, String), (String, String)> {
        let mut last = (KEYLESS.to_owned(), self.empty());
        for vendor in self.order() {
            match vendor.extract(url).await {
                Ok(page) => return Ok((page, vendor.id().to_owned())),
                Err(why) if rotates(&why) => last = (vendor.id().to_owned(), why),
                Err(why) => return Err((vendor.id().to_owned(), why)),
            }
        }
        Err(last)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn a_paid_tier_without_its_key_leaves_the_ring() {
        let mut config = WebConfig::default();
        config
            .provider_tier
            .insert("exa".into(), ProviderTier::Paid);
        let none: SecretLookup = Arc::new(|_| None);
        let client = pmt_net::client(&pmt_net::OutboundPolicy::default()).unwrap();
        let ring = Ring::new(&config, &client, &none, &BTreeMap::new());
        assert_eq!(ring.vendors.len(), 3);
        assert!(ring.problems()[0].contains("EXA_API_KEY"));
        let ids: Vec<&str> = ring.order().map(Vendor::id).collect();
        assert_eq!(ids, ["parallel", "firecrawl", "keenable"]);
        let ids: Vec<&str> = ring.order().map(Vendor::id).collect();
        assert_eq!(ids, ["firecrawl", "keenable", "parallel"]);
    }

    #[test]
    fn only_rate_limits_and_refusals_rotate() {
        assert!(rotates("HTTP 429"));
        assert!(rotates("HTTP 401"));
        assert!(!rotates("HTTP 500"));
        assert!(!rotates("timed out"));
    }
}

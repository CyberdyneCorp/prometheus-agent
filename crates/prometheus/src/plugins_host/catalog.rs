//! The binary's side of the plugin catalog (spec: plugin-system, Plugin
//! catalog; Catalog kill list; Signed plugin packages and provenance):
//! the live catalog fetched from `plugins.catalog_url` and verified
//! against the release keys embedded in the binary, its cache, the kill
//! list every load applies, and package downloads.

use std::time::Duration;

use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_net::OutboundPolicy;
use pmt_plugins::catalog::{self, CACHE_TTL, Cache, Catalog, Removed};

/// How long one catalog or package request may take.
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Verifies a live catalog's signature against the release keys embedded
/// in this binary; answers the key id.
///
/// # Errors
///
/// Why it does not verify.
pub fn verify(bytes: &[u8], signature: &str) -> Result<String, String> {
    pmt_update::TrustRoot::embedded()
        .verify("the plugin catalog", bytes, signature)
        .map(|key| key.id().to_owned())
        .map_err(|e| e.message().to_owned())
}

/// The kill list in force for `scope` without a network call.
#[must_use]
pub fn kill_list(scope: &ProfileScope) -> Vec<Removed> {
    Cache::of(scope.dir()).kill_list(&verify)
}

/// The catalog to resolve names with: the cached live catalog while it is
/// younger than 6 hours (or when `refresh` is false and one is cached),
/// else a fresh fetch; on any failure the embedded catalog, with a WARN
/// naming the failure. The kill list always joins the cached live one.
#[must_use]
pub fn current(scope: &ProfileScope, refresh: bool) -> Catalog {
    let cache = Cache::of(scope.dir());
    let cached = cache.read(&verify);
    if let Some((catalog, age)) = &cached
        && (age < &CACHE_TTL || !refresh)
    {
        return Catalog::merged(Some(catalog.clone()));
    }
    let url = &scope.config().plugins.catalog_url;
    if url.is_empty() || !refresh {
        return Catalog::merged(None);
    }
    match fetch_live(scope, url) {
        Ok((bytes, signature)) => match catalog::accept(&bytes, &signature, &verify) {
            Ok(live) => {
                if let Err(err) = cache.write(&bytes, &signature) {
                    tracing::warn!("plugin catalog not cached: {}", err.message());
                }
                Catalog::merged(Some(live))
            }
            Err(why) => {
                tracing::warn!(url, "plugin catalog ignored, using the embedded one: {why}");
                embedded_with(cached)
            }
        },
        Err(why) => {
            tracing::warn!(
                url,
                "plugin catalog unavailable, using the embedded one: {why}"
            );
            embedded_with(cached)
        }
    }
}

/// The embedded entries with the kill list of a cached live catalog.
fn embedded_with(cached: Option<(Catalog, Duration)>) -> Catalog {
    let mut catalog = Catalog::embedded();
    catalog.removed = Catalog::merged(cached.map(|(c, _)| c)).removed;
    catalog
}

fn fetch_live(scope: &ProfileScope, url: &str) -> Result<(Vec<u8>, String), String> {
    let bytes = download(scope, url)
        .map_err(|e| e.message().to_owned())?
        .ok_or_else(|| format!("{url} was not found"))?;
    if bytes.len() > catalog::MAX_BYTES {
        return Err(format!(
            "the catalog is {} bytes, more than {}",
            bytes.len(),
            catalog::MAX_BYTES
        ));
    }
    let signature = download(scope, &format!("{url}.minisig"))
        .map_err(|e| e.message().to_owned())?
        .ok_or_else(|| "the catalog has no signature".to_owned())?;
    Ok((bytes, String::from_utf8_lossy(&signature).into_owned()))
}

/// Downloads `url` with the profile's network settings; `None` for a 404.
///
/// # Errors
///
/// `plugins.catalog` when the request fails or answers another error.
pub fn download(scope: &ProfileScope, url: &str) -> Result<Option<Vec<u8>>, Error> {
    let network = &scope.config().network;
    let policy = OutboundPolicy {
        force_ipv4: network.force_ipv4,
        extra_ca_file: network.extra_ca_file.as_ref().map(Into::into),
        proxy: network.proxy.clone(),
    };
    let failed = |why: String| Error::new(&codes::PLUGINS_CATALOG, format!("{url}: {why}"));
    let client = pmt_net::http::client_builder(&policy)
        .and_then(|b| {
            b.timeout(FETCH_TIMEOUT)
                .build()
                .map_err(|e| Error::new(&codes::PLUGINS_CATALOG, e.to_string()))
        })
        .map_err(|e| failed(e.message().to_owned()))?;
    let url = url.to_owned();
    crate::model_cmd::block_on(async move {
        let response = client.get(&url).send().await.map_err(|e| e.to_string())?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            return Err(format!("answered {status}"));
        }
        response
            .bytes()
            .await
            .map(|b| Some(b.to_vec()))
            .map_err(|e| e.to_string())
    })?
    .map_err(failed)
}

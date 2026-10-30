//! Automatic provider detection for requested provider `auto` (spec:
//! model-providers, Auto provider detection): the configured provider,
//! then the first provider of `model.auto_order` with a stored secret,
//! then a sign-in, then a reachable configured local endpoint.

use pmt_config::provider_urls::{host_of, is_local_host};
use pmt_core::error::{Error, codes};

use super::Resolver;

/// The exact not-connected message.
pub const NOT_CONNECTED: &str = "Prometheus is not connected to any model provider. Run 'prometheus model' or 'prometheus auth add <provider>'.";

/// The chosen provider name and why.
pub(super) async fn detect(resolver: &Resolver<'_>) -> Result<(String, String), Error> {
    let config = &resolver.config.model;
    if let Some(provider) = config.provider.as_deref().filter(|p| *p != "auto") {
        return Ok((provider.to_owned(), "model.provider".to_owned()));
    }
    if let Some(found) = with_stored_secret(resolver)? {
        return Ok(found);
    }
    if let Some(found) = signed_in(resolver) {
        return Ok(found);
    }
    if let Some(found) = reachable_local(resolver).await {
        return Ok(found);
    }
    Err(Error::new(&codes::PROVIDER_NOT_CONFIGURED, NOT_CONNECTED)
        .with_hint("prometheus secrets set <PROVIDER>_API_KEY"))
}

/// The first `model.auto_order` provider with a stored secret. Entries
/// this registry does not know (a provider of a plugin that is not
/// enabled here) and disabled providers are passed over.
fn with_stored_secret(resolver: &Resolver<'_>) -> Result<Option<(String, String)>, Error> {
    for name in &resolver.config.model.auto_order {
        let Ok(provider) = resolver.registry.enabled(name) else {
            continue;
        };
        for secret in provider.secret_names() {
            if resolver.secrets.secret(&secret)?.is_some() {
                let why = format!("secret {secret} is stored (model.auto_order)");
                return Ok(Some((provider.id().to_owned(), why)));
            }
        }
    }
    Ok(None)
}

fn signed_in(resolver: &Resolver<'_>) -> Option<(String, String)> {
    resolver.extras.signed_in().iter().find_map(|name| {
        let provider = resolver.registry.enabled(name).ok()?;
        Some((provider.id().to_owned(), "signed in".to_owned()))
    })
}

/// The first enabled provider whose configured base URL is local and
/// accepts connections.
async fn reachable_local(resolver: &Resolver<'_>) -> Option<(String, String)> {
    let candidates = resolver
        .registry
        .iter()
        .filter(|p| p.enabled())
        .filter_map(|p| {
            let url = p.settings().base_url.as_deref()?;
            host_of(url)
                .is_some_and(|h| is_local_host(&h))
                .then_some((p.id(), url))
        });
    for (id, url) in candidates {
        if resolver.probe.reachable(url).await {
            return Some((id.to_owned(), format!("local endpoint {url} answers")));
        }
    }
    None
}

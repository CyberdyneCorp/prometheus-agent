//! Web and media categories in the `providers` group (spec:
//! web-and-media-tools, Media tool credentials and billing are explicit;
//! Keyless web search rotation is explicit opt-in): each category's
//! resolved provider, billing source and availability, and keyless web
//! traffic listed as outbound to third parties without an account.

use pmt_config::schema::Config;
use pmt_core::doctor::{Check, Status};
use pmt_media::explain::CategoryView;

use super::Inputs;

/// Whether `category` was selected explicitly (an unavailable explicit
/// choice warns; an unconfigured category does not).
fn explicit(config: &Config, category: &str) -> bool {
    let web = &config.web;
    match category {
        "web_search" => web.search_backend.is_some() || web.backend != "auto",
        "web_extract" => web.extract_backend.is_some() || web.backend != "auto",
        "image_generation" => config.image_gen.provider.is_some(),
        "tts" => config.tts.provider.is_some(),
        "stt" => config.stt.provider.is_some(),
        _ => false,
    }
}

fn line(view: &CategoryView, config: &Config) -> Check {
    let (status, message) = match (&view.provider, &view.billing_source) {
        (Some(provider), Some(billing)) => (
            Status::Ok,
            format!("{}: {provider} ({billing})", view.category),
        ),
        (Some(provider), None) => (
            Status::Ok,
            format!("{}: {provider} ({})", view.category, view.availability),
        ),
        _ if explicit(config, view.category) => (
            Status::Warn,
            format!("{}: {}", view.category, view.availability),
        ),
        _ => (
            Status::Ok,
            format!("{}: {}", view.category, view.availability),
        ),
    };
    Check::new("providers", "media", status, message)
}

/// Whether web calls may go to the keyless ring.
fn keyless(config: &Config) -> bool {
    let web = &config.web;
    let named = [
        Some(&web.backend),
        web.search_backend.as_ref(),
        web.extract_backend.as_ref(),
    ]
    .into_iter()
    .flatten()
    .any(|id| id == "keyless");
    named || web.keyless.enabled
}

/// The media checks of the selected profile.
pub(super) fn checks(inputs: &Inputs<'_>) -> Vec<Check> {
    let scope = inputs.scope;
    let config = scope.config();
    let Ok(registry) = crate::tools_cmd::base_registry(scope) else {
        return Vec::new();
    };
    let mut checks: Vec<Check> = crate::tools_cmd::media_categories(scope, &registry)
        .unwrap_or_default()
        .iter()
        .map(|view| line(view, config))
        .collect();
    if keyless(config) {
        let vendors = pmt_tools::web::keyless::RING.map(|v| v.id).join(", ");
        checks.push(Check::new(
            "providers",
            "keyless",
            Status::Ok,
            format!("keyless web search sends queries and URLs to {vendors} (third parties, without an account)"),
        ));
        // The same rule the session's ring applies (a paid tier without
        // its key leaves the ring), from the ring itself.
        let scope_secrets = scope.clone();
        let secrets: pmt_tools::web::SecretLookup =
            std::sync::Arc::new(move |name| scope_secrets.secrets().resolve(name).ok());
        let client = pmt_net::client(&pmt_net::OutboundPolicy::default()).ok();
        let ring = client.map(|client| {
            pmt_tools::web::keyless::Ring::new(
                &config.web,
                &client,
                &secrets,
                &std::collections::BTreeMap::new(),
            )
        });
        for problem in ring
            .iter()
            .flat_map(pmt_tools::web::keyless::Ring::problems)
        {
            checks.push(Check::new(
                "providers",
                "keyless",
                Status::Warn,
                problem.clone(),
            ));
        }
    }
    checks
}

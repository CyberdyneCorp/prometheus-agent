//! `prometheus model list [--provider ID] [--providers] [--refresh]`
//! (spec: model-providers, prometheus model command; Built-in provider
//! catalog; Context length detection chain): models with their context
//! window and capabilities, or with `--providers` every provider of the
//! profile's registry with its aliases, wire format, endpoint and secret
//! names (never a secret value).

use std::fmt::Write as _;

use pmt_core::error::Error;
use pmt_providers::Route;
use pmt_providers::metadata::{modelsdev, window};
use pmt_providers::registry::{Origin, Provider};
use pmt_providers::resolve::{CredentialExtras as _, CredentialSource, Target, Trace};
use serde::Serialize;

use super::{Models, block_on, client};
use crate::context::Context;
use crate::output::{document, print_raw};

pub(super) fn run(
    ctx: &Context<'_>,
    provider: Option<&str>,
    providers: bool,
    refresh: bool,
) -> Result<u8, Error> {
    let models = Models::open(ctx)?;
    if providers {
        return list_providers(ctx, &models);
    }
    let config = models.scope.config();
    let chosen: Vec<&Provider> = match provider.or(config.model.provider.as_deref()) {
        Some(name) => vec![models.registry.lookup(name)?],
        None => models
            .registry
            .iter()
            .filter(|p| !p.descriptor().hidden)
            .collect(),
    };
    if refresh {
        block_on(refresh_all(&models, &chosen))??;
    } else if provider.is_some() && chosen.iter().any(|p| scoped_unlisted(&models, p)) {
        // A region's or resource's own catalog (Bedrock, Azure AI Foundry)
        // has no built-in entries: list it on first use.
        block_on(refresh_listings(&models, &chosen))??;
    }
    list_models(ctx, &models, &chosen)
}

/// Lists `provider`'s region or resource catalog when none is cached
/// (best effort: a failure is reported and the built-in entries stay),
/// so pickers offer Bedrock's models and a Foundry resource's
/// deployments.
pub(super) fn list_scoped(models: &Models, provider: &Provider) {
    if scoped_unlisted(models, provider)
        && let Ok(Err(err)) = block_on(refresh_listings(models, &[provider]))
    {
        eprintln!("warning: {}: {}", provider.id(), err.message());
    }
}

/// A provider whose models only its live listing knows (Bedrock's region,
/// an Azure AI Foundry resource's deployments) and that has none cached.
fn scoped_unlisted(models: &Models, provider: &Provider) -> bool {
    let scoped = pmt_providers::aws::is_bedrock(provider)
        || provider.descriptor().wire_need == pmt_providers::descriptor::WireNeed::Detected;
    scoped && provider.supports_model_listing() && !models.catalog.has_listing(provider.id())
}

#[derive(Serialize)]
struct ProviderRow<'a> {
    id: &'a str,
    aliases: &'a [String],
    wire_format: &'a str,
    base_url: Option<&'a str>,
    endpoint: &'a str,
    secret_names: Vec<String>,
    auth: &'static str,
    local: bool,
    hidden: bool,
    enabled: bool,
    origin: &'static str,
}

fn list_providers(ctx: &Context<'_>, models: &Models) -> Result<u8, Error> {
    let rows: Vec<ProviderRow<'_>> = models
        .registry
        .iter()
        .map(|p| {
            let d = p.descriptor();
            ProviderRow {
                id: p.id(),
                aliases: &d.aliases,
                wire_format: d.wire_label(),
                base_url: p.base_url(),
                endpoint: p
                    .base_url()
                    .or(d.endpoint_note.as_deref())
                    .unwrap_or("required"),
                secret_names: p.secret_names(),
                auth: p.auth().as_str(),
                local: d.local,
                hidden: d.hidden,
                enabled: p.enabled(),
                origin: match p.origin() {
                    Origin::Builtin => "builtin",
                    Origin::Config => "config",
                    Origin::Plugin => "plugin",
                },
            }
        })
        .collect();
    if ctx.json {
        return document(
            "model list --providers",
            &serde_json::json!({ "providers": rows }),
        );
    }
    let mut text = String::new();
    for row in rows.iter().filter(|r| !r.hidden) {
        let aliases = if row.aliases.is_empty() {
            String::new()
        } else {
            format!(" ({})", row.aliases.join(", "))
        };
        let secrets = if row.secret_names.is_empty() {
            row.auth.to_owned()
        } else {
            row.secret_names.join(", ")
        };
        let _ = writeln!(
            text,
            "{}{aliases}\n    {} at {}; {secrets}",
            row.id, row.wire_format, row.endpoint
        );
    }
    print_raw(&text)
}

/// Fetches each provider's listing and the models.dev catalog.
async fn refresh_all(models: &Models, chosen: &[&Provider]) -> Result<(), Error> {
    refresh_listings(models, chosen).await?;
    let client = client(models.scope.config())?;
    if let Err(err) = models
        .catalog
        .refresh_models_dev(&client, modelsdev::URL)
        .await
    {
        eprintln!("warning: {}", err.message());
    }
    Ok(())
}

/// Fetches each provider's listing (best effort: a provider without a
/// usable credential or endpoint is reported and skipped). A minted
/// credential (AWS chain, Entra ID, Vertex) is minted first.
async fn refresh_listings(models: &Models, chosen: &[&Provider]) -> Result<(), Error> {
    let config = models.scope.config();
    let client = client(config)?;
    let resolver = models.resolver(config, None);
    let now = jiff::Timestamp::now();
    for provider in chosen.iter().filter(|p| p.supports_model_listing()) {
        let target = Target {
            provider: provider.id().to_owned(),
            model: Some(String::new()),
            ..Target::default()
        };
        let refreshed = match resolver
            .resolve_target(&target, &mut Trace::default())
            .await
        {
            Ok(route) => match minted(models, provider, route).await {
                Ok(route) => {
                    models
                        .catalog
                        .refresh_route(&client, provider, &route, now)
                        .await
                }
                Err(err) => Err(err),
            },
            Err(err) => Err(err),
        };
        if let Err(err) = refreshed {
            eprintln!("warning: {}: {}", provider.id(), err.message());
        }
    }
    Ok(())
}

/// `route` with its minted credential's current value.
async fn minted(models: &Models, provider: &Provider, route: Route) -> Result<Route, Error> {
    if !matches!(route.credential.source, CredentialSource::Minted { .. }) {
        return Ok(route);
    }
    match models.accounts.mint(provider).await {
        Some(token) => Ok(Route {
            credential: route.credential.clone().with_value(token?),
            ..route
        }),
        None => Ok(route),
    }
}

// The flags are the metadata's own capability fields.
#[allow(clippy::struct_excessive_bools)]
#[derive(Serialize)]
struct ModelRow {
    provider: String,
    model: String,
    context_window: u64,
    context_source: &'static str,
    supports_tools: bool,
    supports_vision: bool,
    supports_reasoning: bool,
    current: bool,
    /// `deprecated` or `retired` per the retirement table.
    #[serde(skip_serializing_if = "Option::is_none")]
    retirement: Option<&'static str>,
    /// The window as shown (`200000 (pinned)`).
    #[serde(skip)]
    shown: String,
}

fn list_models(ctx: &Context<'_>, models: &Models, chosen: &[&Provider]) -> Result<u8, Error> {
    let config = models.scope.config();
    let current_provider = config
        .model
        .provider
        .as_deref()
        .and_then(|p| models.registry.canonical(p));
    let mut rows = Vec::new();
    for provider in chosen {
        for model in models.model_ids(provider) {
            let (meta, _) = models
                .catalog
                .metadata_at(provider.id(), &model, provider.base_url());
            let retirement = pmt_providers::retirement::lookup(config, provider.id(), &model)
                .map(|entry| entry.status.as_str());
            rows.push(ModelRow {
                retirement,
                shown: window::label(meta.context_window, meta.context_source),
                current: current_provider == Some(provider.id())
                    && config.model.default.as_deref() == Some(model.as_str()),
                provider: provider.id().to_owned(),
                model,
                context_window: meta.context_window,
                context_source: meta.context_source.as_str(),
                supports_tools: meta.supports_tools,
                supports_vision: meta.supports_vision,
                supports_reasoning: meta.supports_reasoning,
            });
        }
    }
    if ctx.json {
        return document("model list", &serde_json::json!({ "models": rows }));
    }
    let mut text = String::new();
    for row in &rows {
        let mut flags = Vec::new();
        for (on, name) in [
            (row.supports_tools, "tools"),
            (row.supports_vision, "vision"),
            (row.supports_reasoning, "reasoning"),
        ] {
            if on {
                flags.push(name);
            }
        }
        let _ = writeln!(
            text,
            "{} {}/{}  ctx {}  {}{}",
            if row.current { "*" } else { " " },
            row.provider,
            row.model,
            row.shown,
            flags.join(","),
            row.retirement
                .map_or_else(String::new, |s| format!("  [{s}]"))
        );
    }
    if rows.is_empty() {
        text.push_str("no models known; run: prometheus model list --refresh\n");
    }
    print_raw(&text)
}

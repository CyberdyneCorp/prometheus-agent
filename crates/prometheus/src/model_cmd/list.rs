//! `prometheus model list [--provider ID] [--providers] [--refresh]`
//! (spec: model-providers, prometheus model command; Built-in provider
//! catalog; Context length detection chain): models with their context
//! window and capabilities, or with `--providers` every provider of the
//! profile's registry with its aliases, wire format, endpoint and secret
//! names (never a secret value).

use std::fmt::Write as _;

use pmt_core::error::Error;
use pmt_providers::metadata::{modelsdev, window};
use pmt_providers::registry::{Origin, Provider};
use pmt_providers::resolve::{Target, Trace};
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
    }
    list_models(ctx, &models, &chosen)
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

/// Fetches each provider's listing (best effort: a provider without a
/// usable credential or endpoint is reported and skipped) and the
/// models.dev catalog.
async fn refresh_all(models: &Models, chosen: &[&Provider]) -> Result<(), Error> {
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
            Ok(route) => {
                models
                    .catalog
                    .refresh(&client, provider, route.credential.value(), now)
                    .await
            }
            Err(err) => Err(err),
        };
        if let Err(err) = refreshed {
            eprintln!("warning: {}: {}", provider.id(), err.message());
        }
    }
    if let Err(err) = models
        .catalog
        .refresh_models_dev(&client, modelsdev::URL)
        .await
    {
        eprintln!("warning: {}", err.message());
    }
    Ok(())
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
        for model in models.catalog.picker_models(provider) {
            let (meta, _) = models
                .catalog
                .metadata_at(provider.id(), &model, provider.base_url());
            rows.push(ModelRow {
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
            "{} {}/{}  ctx {}  {}",
            if row.current { "*" } else { " " },
            row.provider,
            row.model,
            row.shown,
            flags.join(",")
        );
    }
    if rows.is_empty() {
        text.push_str("no models known; run: prometheus model list --refresh\n");
    }
    print_raw(&text)
}

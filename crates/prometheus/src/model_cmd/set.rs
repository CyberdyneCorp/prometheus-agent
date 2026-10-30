//! `prometheus model set <provider>/<model> [--wire-format F] [--base-url
//! URL]` (spec: model-providers, prometheus model command; Local server
//! detection and context probes; User model aliases): the route is
//! validated against the proposed config (credential, tool support,
//! minimum context window) before anything is written; a local server
//! root is normalised to the prefix that serves its model listing and
//! that prefix is saved.

use pmt_config::Config;
use pmt_config::provider_urls;
use pmt_config::schema::WireFormat;
use pmt_core::error::Error;
use pmt_providers::{ProviderRegistry, local, target};
use serde::Serialize;

use super::{Models, block_on, client, validate};
use crate::cli::WireFormatArg;
use crate::context::Context;
use crate::output::{document, print};

#[derive(Serialize)]
struct SetView {
    provider: String,
    model: String,
    base_url: String,
    wire_format: &'static str,
    context_window: u64,
    context_source: &'static str,
    written: Vec<String>,
}

pub(super) fn run(
    ctx: &Context<'_>,
    reference: &str,
    wire_format: Option<WireFormatArg>,
    base_url: Option<&str>,
) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let config = scope.config();
    let view = block_on(set(&scope, config, reference, wire_format, base_url))??;
    if ctx.json {
        return document("model set", &view);
    }
    print(&format!(
        "model set: {}/{} (context window {}; wrote {})",
        view.provider,
        view.model,
        view.context_window,
        view.written.join(", ")
    ))
}

async fn set(
    scope: &pmt_config::ProfileScope,
    config: &Config,
    reference: &str,
    wire_format: Option<WireFormatArg>,
    base_url: Option<&str>,
) -> Result<SetView, Error> {
    let registry = ProviderRegistry::for_profile(config, Vec::new())?;
    let target = target::parse(
        reference,
        &config.model,
        &registry,
        config.model.provider.as_deref(),
    )?;
    let provider = registry.lookup(&target.provider)?;
    let id = provider.id().to_owned();
    let mut written = Vec::new();
    let mut proposed = config.clone();
    let settings = proposed.providers.entry(id.clone()).or_default();
    if let Some(format) = wire_format {
        settings.wire_format = Some(wire(format));
        written.push((
            format!("providers.{id}.wire_format"),
            wire(format).as_str().to_owned(),
        ));
    }
    let configured_url = base_url.map(str::to_owned).or(settings.base_url.clone());
    if let Some(url) = configured_url {
        let url = if provider.descriptor().local {
            local::normalise(&client(config)?, &url).await?
        } else {
            url
        };
        if base_url.is_some() || settings.base_url.as_deref() != Some(url.as_str()) {
            written.push((format!("providers.{id}.base_url"), url.clone()));
        }
        settings.base_url = Some(url);
    }
    provider_urls::check(&proposed)?;
    let models = Models::for_config(scope.clone(), &proposed)?;
    let candidate = models.registry.lookup(&id)?;
    if candidate.descriptor().local
        && let Some(model) = target.model.as_deref()
    {
        local::discover(&client(config)?, &models.catalog, candidate, model).await;
    }
    let resolver = models.resolver(&proposed, None);
    let (route, meta) = validate(&resolver, &models.catalog, &target).await?;
    // An alias stays an alias in config, so its endpoint and secret apply.
    let default = if target::alias(&config.model, reference).is_some() {
        reference.to_owned()
    } else {
        route.model.clone()
    };
    written.insert(0, ("model.default".to_owned(), default));
    written.insert(0, ("model.provider".to_owned(), id));
    let pairs: Vec<(&str, &str)> = written
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    scope.profile().config_file().set_all(&pairs)?;
    Ok(SetView {
        provider: route.provider.clone(),
        model: route.model.clone(),
        base_url: route.base_url.clone(),
        wire_format: route.wire_format.as_str(),
        context_window: meta.context_window,
        context_source: meta.context_source.as_str(),
        written: written.into_iter().map(|(k, _)| k).collect(),
    })
}

fn wire(format: WireFormatArg) -> WireFormat {
    match format {
        WireFormatArg::ChatCompletions => WireFormat::ChatCompletions,
        WireFormatArg::Responses => WireFormat::Responses,
        WireFormatArg::AnthropicMessages => WireFormat::AnthropicMessages,
    }
}

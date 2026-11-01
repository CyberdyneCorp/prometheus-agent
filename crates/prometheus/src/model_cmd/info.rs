//! `prometheus model info` (spec: model-providers, Reasoning effort
//! defaults, overrides and display; Context length detection chain): the
//! route a new session of the profile would use, its context window with
//! its source (a pin labelled `(pinned)`), capabilities and the reasoning
//! effort that would be sent, plus the status-line segment. No model call.

use pmt_core::error::Error;
use pmt_providers::reasoning::{self, Effort};
use pmt_providers::resolve::{Selection, Trace, requested};
use pmt_providers::status;
use serde::Serialize;

use super::{Models, block_on};
use crate::context::Context;
use crate::output::{document, print_raw};

// The flags are the metadata's own capability fields.
#[allow(clippy::struct_excessive_bools)]
#[derive(Serialize)]
struct InfoView {
    profile: String,
    provider: String,
    model: String,
    base_url: String,
    wire_format: &'static str,
    credential: String,
    billing_source: String,
    context_window: u64,
    context_source: &'static str,
    max_output_tokens: Option<u64>,
    supports_tools: bool,
    supports_vision: bool,
    supports_reasoning: bool,
    reasoning: Effort,
    status_line: String,
}

pub(super) fn run(ctx: &Context<'_>) -> Result<u8, Error> {
    let models = Models::open(ctx)?;
    let config = models.scope.config();
    let resolver = models.resolver(config, None);
    let requested = requested(&Selection::default(), &config.model);
    let route = block_on(resolver.resolve(&requested, &mut Trace::default()))??;
    let (meta, _) =
        models
            .catalog
            .metadata_at(&route.provider, &route.model, Some(&route.base_url));
    let effort = reasoning::effort(&config.agent, None, &route.model, &meta)?;
    let view = InfoView {
        profile: models.scope.name().as_str().to_owned(),
        provider: route.provider.clone(),
        model: route.model.clone(),
        base_url: route.base_url.clone(),
        wire_format: route.wire_format.as_str(),
        credential: route.credential.describe(),
        billing_source: route.billing_source(),
        context_window: meta.context_window,
        context_source: meta.context_source.as_str(),
        max_output_tokens: meta.max_output_tokens,
        supports_tools: meta.supports_tools,
        supports_vision: meta.supports_vision,
        supports_reasoning: meta.supports_reasoning,
        status_line: status::line(&route, &meta, &effort),
        reasoning: effort,
    };
    if ctx.json {
        return document("model info", &view);
    }
    let yes = |b: bool| if b { "yes" } else { "no" };
    print_raw(&format!(
        "{}\n\nprovider        {}\nmodel           {}\nendpoint        {} ({})\ncredential      {} (billing {})\ncontext window  {} (from {})\ntools           {}\nvision          {}\nreasoning       {}\n",
        view.status_line,
        view.provider,
        view.model,
        view.base_url,
        view.wire_format,
        view.credential,
        view.billing_source,
        view.context_window,
        view.context_source,
        yes(view.supports_tools),
        yes(view.supports_vision),
        view.reasoning.describe(),
    ))
}

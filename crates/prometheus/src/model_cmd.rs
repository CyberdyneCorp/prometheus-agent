//! `prometheus model explain` (spec: model-providers, Deterministic
//! explainable resolution): runs the route resolution of a new session in
//! the selected profile and prints every step, without a model call and
//! without any secret value (credentials are named by source:
//! `secret:<NAME>`, `pool:<provider>#<n>`, `oauth:<provider>`, `none`).

use std::fmt::Write as _;

use pmt_core::error::{Error, codes};
use pmt_providers::capability;
use pmt_providers::metadata::Catalog;
use pmt_providers::resolve::{
    Layer, NoExtras, Outcome, Requested, Resolver, Selection, Step, TcpProbe, Trace, requested,
};
use pmt_providers::{ProviderRegistry, Route};
use serde::Serialize;

use crate::cli::ModelCommand;
use crate::context::Context;
use crate::output::{document, print_error, print_raw};

/// Runs one `model` subcommand and returns its exit code.
///
/// # Errors
///
/// The command's failure; the caller prints it and exits with its code.
pub fn run(command: &ModelCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    match command {
        ModelCommand::Explain { provider, model } => explain(ctx, provider.clone(), model.clone()),
    }
}

#[derive(Serialize)]
struct RouteView {
    provider: String,
    model: String,
    wire_format: &'static str,
    base_url: String,
    credential: String,
    billing_source: String,
    context_window: u64,
}

#[derive(Serialize)]
struct Explained<'a> {
    profile: &'a str,
    requested: &'a Requested,
    steps: &'a [Step],
    route: Option<RouteView>,
}

fn explain(
    ctx: &Context<'_>,
    provider: Option<String>,
    model: Option<String>,
) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let config = scope.config();
    let registry = ProviderRegistry::for_profile(config, Vec::new())?;
    let selection = Selection {
        flag: Layer { provider, model },
        session: Layer::default(),
    };
    let requested = requested(&selection, &config.model);
    let catalog = Catalog::new(config, Some(scope.path("cache/models")));
    let probe = TcpProbe::default();
    let resolver = Resolver {
        registry: &registry,
        config,
        secrets: scope.secrets(),
        extras: &NoExtras,
        probe: &probe,
        flag_key: None,
    };
    let mut trace = Trace::default();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::new(&codes::OUTPUT_IO, format!("cannot start the resolver: {e}")))?;
    let outcome = runtime
        .block_on(resolver.resolve(&requested, &mut trace))
        .and_then(|route| {
            check_capability(&catalog, route, config.model.min_context_window, &mut trace)
        });
    let view = outcome.as_ref().ok().map(|(route, window)| RouteView {
        provider: route.provider.clone(),
        model: route.model.clone(),
        wire_format: route.wire_format.as_str(),
        base_url: route.base_url.clone(),
        credential: route.credential.describe(),
        billing_source: route.billing_source(),
        context_window: *window,
    });
    let body = Explained {
        profile: scope.name().as_str(),
        requested: &requested,
        steps: &trace.0,
        route: view,
    };
    if ctx.json {
        document("model explain", &body)?;
    } else {
        print_raw(&render(&body))?;
    }
    match outcome {
        Ok(_) => Ok(0),
        Err(err) => {
            // The trace is on stdout; the error document goes to stderr.
            print_error(&err, ctx.json);
            Ok(err.exit_code())
        }
    }
}

/// The capability step: the model's window against
/// `model.min_context_window` (metadata from overrides, cached listing or
/// the built-in catalog; no network).
fn check_capability(
    catalog: &Catalog,
    route: Route,
    min: u64,
    trace: &mut Trace,
) -> Result<(Route, u64), Error> {
    let (meta, sources) = catalog.metadata(&route.provider, &route.model);
    trace.check(
        "capability",
        capability::check_min_context(&route.label(), &meta, min),
    )?;
    let source = serde_json::to_value(sources[0])
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned));
    trace.ok(
        "capability",
        format!(
            "{}-token context window ({}) >= model.min_context_window {min}",
            meta.context_window,
            source.unwrap_or_default()
        ),
    );
    Ok((route, meta.context_window))
}

fn render(body: &Explained<'_>) -> String {
    let mut text = String::new();
    for step in body.steps {
        let mark = match step.outcome {
            Outcome::Ok => "ok",
            Outcome::Failed => "FAILED",
        };
        let _ = writeln!(text, "{:<12} {:<6} {}", step.step, mark, step.detail);
    }
    if let Some(route) = &body.route {
        let _ = writeln!(
            text,
            "\nroute: {}/{} via {} at {} (credential {}, billing {})",
            route.provider,
            route.model,
            route.wire_format,
            route.base_url,
            route.credential,
            route.billing_source
        );
    }
    text
}

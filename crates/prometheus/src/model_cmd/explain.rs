//! `prometheus model explain` (spec: model-providers, Deterministic
//! explainable resolution): runs the route resolution of a new session in
//! the selected profile and prints every step, without a model call and
//! without any secret value (credentials are named by source:
//! `secret:<NAME>`, `pool:<provider>#<n>`, `subscription:<provider>/<label>`, `flag`,
//! `none`).

use std::fmt::Write as _;

use pmt_core::error::Error;
use pmt_providers::Route;
use pmt_providers::capability;
use pmt_providers::metadata::{Catalog, ModelMetadata};
use pmt_providers::resolve::{AdHoc, Layer, Outcome, Requested, Selection, Step, Trace, requested};
use serde::Serialize;

use super::{Models, block_on};
use crate::cli::EndpointArgs;
use crate::context::Context;
use crate::output::{document, print_error, print_raw};

/// The launch-only endpoint of `--base-url` / `--api-key-stdin`; the key
/// is read from stdin into memory and never written anywhere.
pub(crate) fn ad_hoc(endpoint: &EndpointArgs) -> Result<Option<AdHoc>, Error> {
    let Some(base_url) = &endpoint.base_url else {
        return Ok(None);
    };
    let key = if endpoint.api_key_stdin {
        Some(crate::prompt::read_secret("the --base-url API key")?)
    } else {
        None
    };
    AdHoc::new(base_url, key).map(Some)
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
    context_source: &'static str,
}

#[derive(Serialize)]
struct Explained<'a> {
    profile: &'a str,
    requested: &'a Requested,
    steps: &'a [Step],
    route: Option<RouteView>,
}

pub(super) fn run(
    ctx: &Context<'_>,
    provider: Option<String>,
    model: Option<String>,
    endpoint: &EndpointArgs,
) -> Result<u8, Error> {
    let ad_hoc = ad_hoc(endpoint)?;
    let models = Models::open(ctx)?;
    let scope = &models.scope;
    let config = scope.config();
    let selection = Selection {
        flag: Layer { provider, model },
        session: Layer::default(),
    };
    let requested = requested(&selection, &config.model);
    let resolver = models.resolver(config, ad_hoc);
    let mut trace = Trace::default();
    let outcome = block_on(resolver.resolve(&requested, &mut trace))?.and_then(|route| {
        check_capability(
            &models.catalog,
            route,
            config.model.min_context_window,
            &mut trace,
        )
    });
    let view = outcome.as_ref().ok().map(|(route, meta)| RouteView {
        provider: route.provider.clone(),
        model: route.model.clone(),
        wire_format: route.wire_format.as_str(),
        base_url: route.base_url.clone(),
        credential: route.credential.describe(),
        billing_source: route.billing_source(),
        context_window: meta.context_window,
        context_source: meta.context_source.as_str(),
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
) -> Result<(Route, ModelMetadata), Error> {
    let (meta, _) = catalog.metadata_at(&route.provider, &route.model, Some(&route.base_url));
    trace.check(
        "capability",
        capability::check_min_context(&route.label(), &meta, min),
    )?;
    trace.ok(
        "capability",
        format!(
            "{}-token context window (from {}) >= model.min_context_window {min}",
            meta.context_window,
            meta.context_source.as_str()
        ),
    );
    Ok((route, meta))
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

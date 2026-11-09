//! `web_search` and `web_extract` as registry entries.

use std::sync::Arc;

use pmt_config::schema::{Config, WireFormat};
use pmt_core::capability::Capability as Cap;
use serde_json::{Value, json};

use super::{Capability, resolve};
use crate::descriptor::{
    CallContext, Effect, HandlerFuture, Owner, RegisteredTool, ResourceClaims, ToolDescriptor,
    ToolHandler,
};
use crate::environments::{Environments, part};
use crate::probe::FnProbe;
use crate::variants::RouteTraits;

/// The toolset of both tools.
pub const TOOLSET: &str = "web";
/// Most hits `web_search` returns.
pub const MAX_RESULTS: usize = 100;
/// Hits `web_search` returns by default.
pub const DEFAULT_RESULTS: usize = 5;
/// Most URLs per `web_extract` call.
pub const MAX_URLS: usize = 5;
/// Bounds of the per-page character budget.
pub const CHAR_LIMIT_RANGE: (usize, usize) = (2_000, 500_000);

struct SearchHandler(Arc<Environments>);
struct ExtractHandler(Arc<Environments>);

/// `web_search` and `web_extract`, each probed for a configured backend.
#[must_use]
pub fn tools(environments: &Arc<Environments>) -> Vec<RegisteredTool> {
    let probe = |capability: Capability, id: &str| {
        Arc::new(FnProbe::new(
            id,
            move |context: crate::probe::ProbeContext<'_>| {
                let web = &context.config.web;
                let id = super::select::configured(web, capability);
                // An id this build does not implement may name a plugin's
                // provider, which only the session knows: the tool is
                // offered and a call answers `<category> provider '<id>'
                // is not installed or not enabled` when nothing provides it.
                if id != "auto" && !super::BACKENDS.contains(&id) {
                    return Ok(());
                }
                resolve(web, capability, context.has_secret).map(|_| ())
            },
        ))
    };
    vec![
        RegisteredTool::new(
            search_descriptor(),
            Owner::Builtin,
            Arc::new(SearchHandler(Arc::clone(environments))),
        )
        .with_probe(probe(Capability::Search, "web.search_backend"))
        .with_route_check(Arc::new(native_route))
        .with_parameters(Arc::new(native_parameters)),
        RegisteredTool::new(
            extract_descriptor(),
            Owner::Builtin,
            Arc::new(ExtractHandler(Arc::clone(environments))),
        )
        .with_probe(probe(Capability::Extract, "web.extract_backend")),
    ]
}

impl ToolHandler for SearchHandler {
    fn call<'a>(&'a self, context: &'a CallContext, arguments: Value) -> HandlerFuture<'a> {
        Box::pin(async move {
            let web = part(&self.0, context, "web", |env| env.web.clone())?;
            let query = arguments
                .get("query")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let limit = arguments
                .get("limit")
                .and_then(Value::as_u64)
                .and_then(|n| usize::try_from(n).ok())
                .unwrap_or(DEFAULT_RESULTS)
                .clamp(1, MAX_RESULTS);
            let result = web.search(query, limit).await;
            crate::usage::record(web.usage.as_ref(), context, "web_search", &result);
            Ok(result)
        })
    }
}

impl ToolHandler for ExtractHandler {
    fn call<'a>(&'a self, context: &'a CallContext, arguments: Value) -> HandlerFuture<'a> {
        Box::pin(async move {
            let web = part(&self.0, context, "web", |env| env.web.clone())?;
            let urls: Vec<String> = arguments
                .get("urls")
                .and_then(Value::as_array)
                .map(|list| {
                    list.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            let char_limit = arguments
                .get("char_limit")
                .and_then(Value::as_u64)
                .and_then(|n| usize::try_from(n).ok());
            let result = web.extract(&urls, char_limit).await;
            crate::usage::record(web.usage.as_ref(), context, "web_extract", &result);
            Ok(result)
        })
    }
}

/// The stored text format.
pub(crate) const FORMAT: &str = "markdown";

/// The backend that hands web search to the model provider.
pub const OPENAI_NATIVE: &str = "openai-native";

/// The provider whose Responses route runs `openai-native` searches.
pub const NATIVE_PROVIDER: &str = "openai-codex";

/// The schema marker the Responses wire turns into the provider-executed
/// web search tool.
pub const PROVIDER_EXECUTED: &str = "x-provider-executed";

fn native(config: &Config) -> bool {
    super::select::configured(&config.web, Capability::Search) == OPENAI_NATIVE
}

/// `openai-native` works only on an `openai-codex` Responses route; no
/// other backend is substituted (spec: web-and-media-tools, Built-in web
/// backends with capability flags).
fn native_route(config: &Config, route: &RouteTraits) -> Result<(), String> {
    if !native(config) {
        return Ok(());
    }
    let codex = route.provider.as_deref() == Some(NATIVE_PROVIDER)
        && route.wire_format == Some(WireFormat::Responses);
    if codex {
        Ok(())
    } else {
        Err(format!(
            "web.search_backend 'openai-native' needs the {NATIVE_PROVIDER} Responses route as the main model route"
        ))
    }
}

/// On its route, `openai-native` replaces the client-side definition one
/// for one: the Responses wire sends the provider's own web search tool.
fn native_parameters(parameters: &Value, config: &Config) -> Value {
    if native(config) {
        json!({"type": "object", PROVIDER_EXECUTED: "web_search"})
    } else {
        parameters.clone()
    }
}

fn search_descriptor() -> ToolDescriptor {
    let mut d = ToolDescriptor::new(
        "web_search",
        TOOLSET,
        "Search the web. The query goes to the search backend verbatim, so operators \
         such as site:, filetype:, intitle:, -term and quoted phrases work where the \
         backend supports them. Returns ranked results with title, url and snippet.",
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "The search query."},
                "limit": {"type": "integer", "minimum": 1, "maximum": MAX_RESULTS, "description": "Most results (default 5)."}
            },
            "required": ["query"],
            "additionalProperties": false
        }),
    );
    web_traits(&mut d);
    d
}

fn extract_descriptor() -> ToolDescriptor {
    let mut d = ToolDescriptor::new(
        "web_extract",
        TOOLSET,
        "Fetch up to 5 web pages as Markdown (images become [IMAGE: alt] placeholders). \
         Each URL gets its own status; a failing URL does not fail the others. Private \
         network and cloud metadata addresses are refused.",
        json!({
            "type": "object",
            "properties": {
                "urls": {"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": MAX_URLS, "description": "http(s) URLs."},
                "char_limit": {"type": "integer", "minimum": CHAR_LIMIT_RANGE.0, "description": "Characters per page (default web.extract_char_limit)."}
            },
            "required": ["urls"],
            "additionalProperties": false
        }),
    );
    web_traits(&mut d);
    d
}

fn web_traits(descriptor: &mut ToolDescriptor) {
    descriptor.effect = Effect::None;
    descriptor.idempotent = true;
    descriptor.claims = Some(ResourceClaims::none());
    descriptor.capabilities = vec![Cap::parse("net.connect:*").expect("valid capability")];
}

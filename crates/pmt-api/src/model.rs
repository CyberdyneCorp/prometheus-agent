//! Per-request model selection (spec: api-server, Per-request model
//! selection). A turn's route is, in order: the session's own `/model`
//! choice, an `api_server.model_routes` alias named by `model`, the
//! request's `provider` (with its `model`), then the profile's default.
//! Generic clients send a bare `model` such as `gpt-4o`; on Chat
//! Completions and Responses it is ignored unless
//! `api_server.direct_model_requests = true`.

use serde_json::{Value, json};

use crate::Shared;
use crate::auth::Caller;
use crate::host::{ModelChoice, ModelOptions};
use crate::http::{Reply, bad_request};
use crate::turn::Runtime;

/// The route a request asked for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Selected {
    /// The route for this turn's model calls; `None` keeps the session's.
    pub choice: Option<ModelChoice>,
    /// `session`, `route`, `request` or `global`.
    pub route_source: &'static str,
    /// What the request named, for `runtime.requested`.
    pub requested: Option<Value>,
    pub options: ModelOptions,
}

impl Selected {
    /// Where the route came from, as stored with a run and reported in
    /// `runtime`: `{"route_source", "requested"}`.
    pub(crate) fn route(&self) -> Value {
        json!({"route_source": self.route_source, "requested": self.requested})
    }
}

/// The `runtime` block of a finished turn: the provider and model that
/// answered, where that route came from and what the request named, so a
/// generic client can see its own `model` was ignored (spec: api-server,
/// Per-request model selection).
pub(crate) fn runtime(runtime: &Runtime, route: &Value) -> Value {
    let mut block = json!({"provider": runtime.provider, "model": runtime.model,
        "route_source": route["route_source"]});
    if !route["requested"].is_null() {
        block["requested"] = route["requested"].clone();
    }
    block
}

/// Picks the route of a request with `body`; `generic` for Chat
/// Completions and Responses.
pub(crate) async fn select(
    shared: &Shared,
    caller: &Caller,
    body: &Value,
    session: Option<&str>,
    generic: bool,
) -> Result<Selected, Reply> {
    let Some(scope) = shared.host.api_scope(&caller.profile) else {
        return Err(bad_request(
            "profile_unavailable",
            "the profile is not served",
        ));
    };
    let config = scope.config();
    let api = &config.api_server;
    let advertised = api.model_id(&caller.profile);
    let model = body["model"]
        .as_str()
        .filter(|m| !m.is_empty() && *m != advertised)
        .map(str::to_owned);
    let provider = body["provider"]
        .as_str()
        .filter(|p| !p.is_empty())
        .map(str::to_owned);
    let requested = (model.is_some() || provider.is_some())
        .then(|| json!({"model": model, "provider": provider}));
    let mut selected = Selected {
        choice: None,
        route_source: "global",
        requested,
        options: options(body),
    };
    if let Some(alias) = model.as_ref().and_then(|m| api.model_routes.get(m)) {
        if let Some(provider) = provider.as_ref().filter(|p| **p != alias.provider) {
            return Err(bad_request(
                "route_conflict",
                &format!(
                    "provider '{provider}' conflicts with route '{}', which uses provider '{}'",
                    model.as_deref().unwrap_or_default(),
                    alias.provider
                ),
            ));
        }
        selected.choice = Some(ModelChoice {
            provider: alias.provider.clone(),
            model: alias.model.clone(),
        });
        selected.route_source = "route";
    } else if let Some(provider) = provider {
        selected.choice = Some(ModelChoice { provider, model });
        selected.route_source = "request";
    } else if let Some(model) = model.filter(|_| !generic || api.direct_model_requests) {
        let provider = config.model.provider.clone().unwrap_or_default();
        selected.choice = Some(ModelChoice {
            provider,
            model: Some(model),
        });
        selected.route_source = "request";
    }
    if let Some(choice) = &selected.choice {
        match shared
            .host
            .api_check_route(&caller.profile, session, choice)
            .await
        {
            Ok(true) => {}
            Ok(false) => {
                selected.choice = None;
                selected.route_source = "session";
            }
            Err(err) => return Err(bad_request("unknown_provider", err.message())),
        }
    }
    Ok(selected)
}

/// `model_options`, or the OpenAI spellings (`reasoning_effort`,
/// `reasoning.effort`, `service_tier`).
fn options(body: &Value) -> ModelOptions {
    let text = |v: &Value| v.as_str().map(str::to_owned);
    let given = &body["model_options"];
    ModelOptions {
        reasoning_effort: text(&given["reasoning_effort"])
            .or_else(|| text(&body["reasoning_effort"]))
            .or_else(|| text(&body["reasoning"]["effort"])),
        service_tier: text(&given["service_tier"]).or_else(|| text(&body["service_tier"])),
    }
}

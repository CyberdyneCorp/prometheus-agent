//! Local inference servers (spec: model-providers, Local server detection
//! and context probes): root URL normalisation, native model listings
//! (LM Studio `/api/v1/models`, Ollama `/api/tags`, OpenAI-compatible
//! `/v1/models`), served-window probes (Ollama `/api/show` honouring
//! `num_ctx`, llama.cpp `/props` `n_ctx`, vLLM/SGLang `max_model_len`, LM
//! Studio `/api/v0/models/<id>`), and the session-start warnings (an Ollama
//! `num_ctx` of 2048 or less, no tool-calling support) given before the
//! minimum-window check.

use std::collections::HashSet;

use pmt_core::error::{Error, codes};
use serde_json::{Value, json};

use crate::capability;
use crate::metadata::{Catalog, ModelMetadata};
use crate::resolve::Route;

/// The largest Ollama `num_ctx` that is warned about.
pub const SMALL_NUM_CTX: u64 = 2_048;

/// The server software behind a local provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerKind {
    /// LM Studio.
    LmStudio,
    /// Ollama.
    Ollama,
    /// llama.cpp `llama-server`.
    LlamaCpp,
    /// vLLM, `SGLang` and other OpenAI-compatible servers.
    OpenAiCompatible,
}

impl ServerKind {
    /// The server of provider `id`.
    #[must_use]
    pub fn of(id: &str) -> Self {
        match id {
            "lmstudio" => Self::LmStudio,
            "ollama" => Self::Ollama,
            "llamacpp" => Self::LlamaCpp,
            _ => Self::OpenAiCompatible,
        }
    }
}

/// The server root of `base_url` (without a trailing `/v1`).
#[must_use]
pub fn root(base_url: &str) -> &str {
    let url = base_url.trim_end_matches('/');
    url.strip_suffix("/v1").unwrap_or(url)
}

/// The OpenAI-compatible prefix that serves the model listing: `base_url`
/// when it ends in `/v1`, else `<root>/v1` and then the given root are
/// tried. The result is what should be persisted and used for requests.
///
/// # Errors
///
/// `local.server_probe` when neither candidate serves a listing.
pub async fn normalise(client: &reqwest::Client, base_url: &str) -> Result<String, Error> {
    let given = base_url.trim_end_matches('/').to_owned();
    let candidates = if given.ends_with("/v1") {
        vec![given.clone()]
    } else {
        vec![format!("{given}/v1"), given.clone()]
    };
    for candidate in &candidates {
        let listed = get_json(client, &format!("{candidate}/models")).await;
        if listed.is_some_and(|body| !model_ids(&body).is_empty() || body.get("data").is_some()) {
            return Ok(candidate.clone());
        }
    }
    Err(Error::new(
        &codes::LOCAL_SERVER_PROBE,
        format!(
            "no model listing at {}",
            candidates
                .iter()
                .map(|c| format!("{c}/models"))
                .collect::<Vec<_>>()
                .join(" or ")
        ),
    )
    .with_hint("check that the server runs and that the base URL points at it"))
}

/// The models a local server serves, through its native API when it has
/// one, else `<base_url>/models`.
///
/// # Errors
///
/// `local.server_probe` when no listing answers.
pub async fn list_models(
    client: &reqwest::Client,
    kind: ServerKind,
    base_url: &str,
) -> Result<Vec<String>, Error> {
    let native = match kind {
        ServerKind::LmStudio => Some(format!("{}/api/v1/models", root(base_url))),
        ServerKind::Ollama => Some(format!("{}/api/tags", root(base_url))),
        ServerKind::LlamaCpp | ServerKind::OpenAiCompatible => None,
    };
    let compatible = format!("{}/models", base_url.trim_end_matches('/'));
    for url in native.iter().chain(std::iter::once(&compatible)) {
        if let Some(body) = get_json(client, url).await {
            let ids = model_ids(&body);
            if !ids.is_empty() || body.get("data").is_some() {
                return Ok(ids);
            }
        }
    }
    Err(Error::new(
        &codes::LOCAL_SERVER_PROBE,
        format!("no model listing at {compatible}"),
    ))
}

/// Model ids of a listing: `data[].id`, `models[].key|name|id`.
fn model_ids(body: &Value) -> Vec<String> {
    let items = body
        .get("data")
        .or_else(|| body.get("models"))
        .and_then(Value::as_array);
    items
        .into_iter()
        .flatten()
        .filter_map(|item| {
            ["id", "key", "name", "model"]
                .iter()
                .find_map(|k| item.get(*k).and_then(Value::as_str))
                .map(str::to_owned)
        })
        .collect()
}

/// What a context probe found.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Probe {
    /// The window the server runs the model with.
    pub context_window: Option<u64>,
    /// Ollama's `num_ctx` parameter, when set.
    pub num_ctx: Option<u64>,
    /// Whether the model calls tools, when the server says.
    pub supports_tools: Option<bool>,
}

/// Probes the window `model` is served with.
///
/// # Errors
///
/// `local.server_probe` when the server does not answer the probe.
pub async fn probe(
    client: &reqwest::Client,
    kind: ServerKind,
    base_url: &str,
    model: &str,
) -> Result<Probe, Error> {
    let found = match kind {
        ServerKind::Ollama => {
            let url = format!("{}/api/show", root(base_url));
            post_json(client, &url, &json!({"model": model}))
                .await
                .map(|body| ollama(&body))
        }
        ServerKind::LlamaCpp => get_json(client, &format!("{}/props", root(base_url)))
            .await
            .map(|body| Probe {
                context_window: body
                    .pointer("/default_generation_settings/n_ctx")
                    .or_else(|| body.get("n_ctx"))
                    .and_then(Value::as_u64),
                ..Probe::default()
            }),
        ServerKind::LmStudio => {
            let url = format!("{}/api/v0/models/{model}", root(base_url));
            get_json(client, &url).await.map(|body| lm_studio(&body))
        }
        ServerKind::OpenAiCompatible => {
            let url = format!("{}/models", base_url.trim_end_matches('/'));
            get_json(client, &url)
                .await
                .map(|body| served(&body, model))
        }
    };
    found.ok_or_else(|| {
        Error::new(
            &codes::LOCAL_SERVER_PROBE,
            format!("the server at {base_url} did not answer a context probe for '{model}'"),
        )
    })
}

/// Ollama `/api/show`: `num_ctx` from the parameters (what the model runs
/// with) wins over the architecture's `context_length`.
fn ollama(body: &Value) -> Probe {
    let num_ctx = body
        .get("parameters")
        .and_then(Value::as_str)
        .and_then(|params| {
            params.lines().find_map(|line| {
                let mut words = line.split_whitespace();
                (words.next() == Some("num_ctx")).then(|| words.next()?.parse().ok())?
            })
        });
    let trained = body
        .get("model_info")
        .and_then(Value::as_object)
        .and_then(|info| {
            info.iter()
                .find(|(k, _)| k.ends_with(".context_length"))
                .and_then(|(_, v)| v.as_u64())
        });
    let supports_tools = body
        .get("capabilities")
        .and_then(Value::as_array)
        .map(|caps| caps.iter().any(|c| c == "tools"));
    Probe {
        context_window: num_ctx.or(trained),
        num_ctx,
        supports_tools,
    }
}

/// LM Studio `/api/v0/models/<id>`: the loaded window, else the maximum.
fn lm_studio(body: &Value) -> Probe {
    Probe {
        context_window: body
            .get("loaded_context_length")
            .or_else(|| body.get("max_context_length"))
            .and_then(Value::as_u64),
        num_ctx: None,
        supports_tools: body
            .get("capabilities")
            .and_then(Value::as_array)
            .map(|caps| caps.iter().any(|c| c == "tool_use")),
    }
}

/// vLLM/SGLang `/v1/models`: the listed model's `max_model_len`.
fn served(body: &Value, model: &str) -> Probe {
    let window = crate::metadata::listing::parse(body)
        .into_iter()
        .find(|m| m.id == model)
        .and_then(|m| m.context_window);
    Probe {
        context_window: window,
        ..Probe::default()
    }
}

/// Tests `provider`'s endpoint for `model` (best effort): refreshes its
/// model listing into `catalog` and records the served window a probe
/// reports. Returns the probe, when the server answered one.
pub async fn discover(
    client: &reqwest::Client,
    catalog: &Catalog,
    provider: &crate::registry::Provider,
    model: &str,
) -> Option<Probe> {
    let base_url = provider.base_url()?;
    if let Err(err) = catalog
        .refresh(client, provider, None, jiff::Timestamp::now())
        .await
    {
        tracing::debug!("local model listing failed: {}", err.message());
    }
    let found = probe(client, ServerKind::of(provider.id()), base_url, model)
        .await
        .ok()?;
    if let Some(window) = found.context_window {
        catalog.record_probe(provider.id(), model, base_url, window);
    }
    Some(found)
}

/// Session-start warnings, each given once per session.
#[derive(Debug, Default)]
pub struct Warnings {
    given: HashSet<String>,
}

impl Warnings {
    /// The warnings `probe` raises for `provider`/`model` that this session
    /// has not given yet; each is also logged at WARN.
    pub fn check(&mut self, kind: ServerKind, route_label: &str, probe: &Probe) -> Vec<String> {
        let mut found = Vec::new();
        if kind == ServerKind::Ollama
            && let Some(num_ctx) = probe.num_ctx.filter(|n| *n <= SMALL_NUM_CTX)
        {
            found.push(format!(
                "Ollama runs {route_label} with num_ctx = {num_ctx}, too small for an agent; raise num_ctx (for example PARAMETER num_ctx 65536 in a Modelfile, or OLLAMA_CONTEXT_LENGTH)"
            ));
        }
        if probe.supports_tools == Some(false) {
            found.push(format!(
                "the local model {route_label} reports no tool-calling support; tool calls will likely fail"
            ));
        }
        found.retain(|w| self.given.insert(w.clone()));
        for warning in &found {
            tracing::warn!("{warning}");
        }
        found
    }
}

/// Session start on a local route: probe the served window (best effort),
/// record it in the catalog, give the warnings, then apply the
/// `model.min_context_window` check. Returns the warnings given (before
/// any refusal) and the metadata or the refusal.
pub async fn prepare_session(
    client: &reqwest::Client,
    catalog: &Catalog,
    route: &Route,
    min_context_window: u64,
    warnings: &mut Warnings,
) -> (Vec<String>, Result<ModelMetadata, Error>) {
    let mut given = Vec::new();
    if route.local {
        let kind = ServerKind::of(&route.provider);
        match probe(client, kind, &route.base_url, &route.model).await {
            Ok(found) => {
                if let Some(window) = found.context_window {
                    catalog.record_probe(&route.provider, &route.model, &route.base_url, window);
                }
                given = warnings.check(kind, &route.label(), &found);
            }
            Err(err) => tracing::debug!("local context probe failed: {}", err.message()),
        }
    }
    let (meta, _) = catalog.metadata_at(&route.provider, &route.model, Some(&route.base_url));
    let checked = capability::check_min_context(&route.label(), &meta, min_context_window);
    (given, checked.map(|()| meta))
}

async fn get_json(client: &reqwest::Client, url: &str) -> Option<Value> {
    read_json(client.get(url).send().await.ok()?).await
}

async fn post_json(client: &reqwest::Client, url: &str, body: &Value) -> Option<Value> {
    let request = client
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body.to_string());
    read_json(request.send().await.ok()?).await
}

async fn read_json(response: reqwest::Response) -> Option<Value> {
    if !response.status().is_success() {
        return None;
    }
    serde_json::from_str(&response.text().await.ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roots_drop_a_trailing_v1() {
        assert_eq!(root("http://127.0.0.1:1234/v1/"), "http://127.0.0.1:1234");
        assert_eq!(root("http://127.0.0.1:1234"), "http://127.0.0.1:1234");
    }

    #[test]
    fn ollama_num_ctx_wins_over_the_trained_window() {
        let body = json!({
            "parameters": "stop \"<|im_end|>\"\nnum_ctx                        2048",
            "model_info": {"qwen3.context_length": 40960},
            "capabilities": ["completion", "tools"]
        });
        assert_eq!(
            ollama(&body),
            Probe {
                context_window: Some(2_048),
                num_ctx: Some(2_048),
                supports_tools: Some(true)
            }
        );
        let unset = json!({"model_info": {"llama.context_length": 131_072}, "capabilities": ["completion"]});
        let probe = ollama(&unset);
        assert_eq!(probe.context_window, Some(131_072));
        assert_eq!(probe.supports_tools, Some(false));
    }

    #[test]
    fn warnings_are_given_once_per_session() {
        let mut warnings = Warnings::default();
        let small = Probe {
            context_window: Some(2_048),
            num_ctx: Some(2_048),
            supports_tools: Some(true),
        };
        let first = warnings.check(ServerKind::Ollama, "ollama/qwen3", &small);
        assert_eq!(first.len(), 1);
        assert!(first[0].contains("num_ctx"));
        assert!(
            warnings
                .check(ServerKind::Ollama, "ollama/qwen3", &small)
                .is_empty()
        );
    }
}

//! OpenRouter's live image catalog (spec: web-and-media-tools, Built-in
//! image generation providers and models: `openrouter` (live image
//! catalog)).
//!
//! `GET <base>/models` lists every model with its modalities: the image
//! models are those whose `architecture.output_modalities` include
//! `image`, and one edits when its `input_modalities` include `image` too.
//! The catalog states no reference-image limit, so a model of the built-in
//! list keeps its own and any other edit-capable model takes
//! [`REFERENCE_LIMIT`]. The listing is cached process-wide for [`TTL`]:
//! the tool's schema and description are built synchronously from the
//! configuration, so a session primes the cache ([`prime`]) before its
//! surface is built and each call refreshes a stale one.

use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock, PoisonError, RwLock};
use std::time::{Duration, Instant};

use pmt_config::image_models::image_provider;
use serde_json::Value;

use super::Model;

/// The provider id.
pub const PROVIDER: &str = "openrouter";

/// How long a fetched catalog is used before it is fetched again.
pub const TTL: Duration = Duration::from_mins(15);

/// The longest a session start or a call waits for the catalog.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// Reference images an edit-capable model outside the built-in list
/// takes (Gemini Flash Image's limit, the smallest of the listed ones).
pub const REFERENCE_LIMIT: u32 = 3;

/// The image models of a fetched catalog: id -> takes image input.
type Models = BTreeMap<String, bool>;

struct Fetched {
    at: Instant,
    models: Arc<Models>,
}

static CATALOG: LazyLock<RwLock<Option<Fetched>>> = LazyLock::new(RwLock::default);

/// The image models of a `/models` answer: id -> takes image input.
#[must_use]
pub fn image_models(answer: &Value) -> Models {
    let has = |model: &Value, key: &str| {
        model
            .pointer(&format!("/architecture/{key}"))
            .and_then(Value::as_array)
            .is_some_and(|list| list.iter().any(|m| m == "image"))
    };
    answer
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|model| has(model, "output_modalities"))
        .filter_map(|model| {
            let id = model.get("id")?.as_str()?.to_owned();
            Some((id, has(model, "input_modalities")))
        })
        .collect()
}

/// The cached catalog, fresh or not.
fn cached() -> Option<Arc<Models>> {
    CATALOG
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .map(|fetched| Arc::clone(&fetched.models))
}

/// Whether the cached catalog is younger than [`TTL`].
#[must_use]
pub fn fresh() -> bool {
    CATALOG
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .is_some_and(|fetched| fetched.at.elapsed() < TTL)
}

/// Fetches the catalog from `base` (the API root, e.g.
/// `https://openrouter.ai/api/v1`) and caches it.
///
/// # Errors
///
/// The request failed, timed out or the answer is not a model listing.
pub async fn refresh(client: &reqwest::Client, base: &str) -> Result<(), String> {
    let sent = client
        .get(format!("{base}/models?output_modalities=image"))
        .timeout(FETCH_TIMEOUT)
        .send()
        .await;
    let answer = crate::http::ok_json(sent)
        .await
        .map_err(|e| format!("the OpenRouter catalog could not be fetched: {e}"))?;
    if answer.get("data").and_then(Value::as_array).is_none() {
        return Err("the OpenRouter catalog is not a model listing".to_owned());
    }
    *CATALOG.write().unwrap_or_else(PoisonError::into_inner) = Some(Fetched {
        at: Instant::now(),
        models: Arc::new(image_models(&answer)),
    });
    Ok(())
}

/// Fetches the catalog unless a fresh one is cached; a failure keeps the
/// previous one (or none) and is logged, never fatal.
pub async fn prime(client: &reqwest::Client, base: &str) {
    if fresh() {
        return;
    }
    if let Err(why) = refresh(client, base).await {
        tracing::warn!(component = "media", error = %why, "openrouter image catalog unavailable");
    }
}

/// The model `id` as the live catalog describes it: editing from its
/// input modalities, the reference limit of the built-in list when it
/// names the model; `None` before a catalog was fetched or when the
/// catalog does not list `id` as an image model.
#[must_use]
pub fn model(id: &str) -> Option<Model> {
    let edits = *cached()?.get(id)?;
    let listed = image_provider(PROVIDER).and_then(|p| p.model(id));
    let limit = listed.map_or(REFERENCE_LIMIT, |m| m.max_reference_images.max(1));
    Some(Model {
        id: id.to_owned(),
        api_model: id.to_owned(),
        quality: None,
        supports_edit: edits,
        max_reference_images: if edits { limit } else { 0 },
        upscaler: false,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn image_models_are_those_that_output_images_and_edit_with_image_input() {
        let answer = json!({"data": [
            {"id": "a/painter", "architecture": {"input_modalities": ["text", "image"], "output_modalities": ["image", "text"]}},
            {"id": "b/sketcher", "architecture": {"input_modalities": ["text"], "output_modalities": ["image"]}},
            {"id": "c/chat", "architecture": {"input_modalities": ["text", "image"], "output_modalities": ["text"]}},
            {"id": "d/broken"}
        ]});
        let models = image_models(&answer);
        assert_eq!(
            models.into_iter().collect::<Vec<_>>(),
            vec![
                ("a/painter".to_owned(), true),
                ("b/sketcher".to_owned(), false)
            ]
        );
    }
}

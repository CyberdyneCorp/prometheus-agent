//! The models.dev catalog snapshot (spec: model-providers, Context length
//! detection chain): context windows by provider and model in the
//! `https://models.dev/api.json` format. A small snapshot is bundled with
//! the binary; `prometheus model list --refresh` downloads the full one into
//! the profile's `cache/models/models.dev.json`, which later lookups prefer.

use std::collections::BTreeMap;
use std::path::Path;

use pmt_core::error::{Error, codes};
use serde_json::Value;

/// Where `prometheus model list --refresh` downloads the catalog.
pub const URL: &str = "https://models.dev/api.json";

/// The snapshot shipped with the binary.
const BUNDLED: &str = include_str!("models_dev.json");

/// The cache file under the catalog's cache directory.
pub const CACHE_FILE: &str = "models.dev.json";

/// Prometheus provider ids whose models.dev id differs.
const PROVIDER_IDS: &[(&str, &str)] = &[
    ("openai-api", "openai"),
    ("gemini", "google"),
    ("kimi-coding", "moonshotai"),
    ("kimi-coding-cn", "moonshotai-cn"),
    ("alibaba", "alibaba"),
    ("huggingface", "huggingface"),
    ("nvidia", "nvidia"),
];

/// Context windows by models.dev provider id, then model id.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelsDev {
    windows: BTreeMap<String, BTreeMap<String, u64>>,
}

impl ModelsDev {
    /// The cached snapshot under `cache_dir`, else the bundled one.
    ///
    /// # Panics
    ///
    /// Never: the bundled snapshot is valid JSON (a unit test reads it).
    #[must_use]
    pub fn load(cache_dir: Option<&Path>) -> Self {
        let cached = cache_dir
            .and_then(|dir| std::fs::read_to_string(dir.join(CACHE_FILE)).ok())
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .map(|v| Self::parse(&v))
            .filter(|s| !s.windows.is_empty());
        cached.unwrap_or_else(|| {
            Self::parse(&serde_json::from_str(BUNDLED).expect("the bundled snapshot is JSON"))
        })
    }

    /// Reads an `api.json` document.
    #[must_use]
    pub fn parse(doc: &Value) -> Self {
        let mut windows = BTreeMap::new();
        for (provider, entry) in doc.as_object().into_iter().flatten() {
            let models: BTreeMap<String, u64> = entry
                .get("models")
                .and_then(Value::as_object)
                .into_iter()
                .flatten()
                .filter_map(|(id, m)| Some((id.clone(), m.pointer("/limit/context")?.as_u64()?)))
                .filter(|(_, window)| *window > 0)
                .collect();
            if !models.is_empty() {
                windows.insert(provider.clone(), models);
            }
        }
        Self { windows }
    }

    /// The window of `model` at `provider` (a Prometheus id); the model's
    /// window at any provider when `provider` is not in the snapshot.
    #[must_use]
    pub fn context_window(&self, provider: &str, model: &str) -> Option<u64> {
        let id = PROVIDER_IDS
            .iter()
            .find(|(ours, _)| *ours == provider)
            .map_or(provider, |(_, theirs)| theirs);
        if let Some(models) = self.windows.get(id) {
            return models.get(model).copied();
        }
        let bare = model.rsplit('/').next().unwrap_or(model);
        self.windows
            .values()
            .find_map(|models| models.get(model).or_else(|| models.get(bare)))
            .copied()
    }
}

/// Downloads and parses the catalog at `url`.
///
/// # Errors
///
/// `provider.call_failed` naming the URL.
pub async fn fetch(client: &reqwest::Client, url: &str) -> Result<ModelsDev, Error> {
    let failed = |why: String| {
        Error::new(
            &codes::PROVIDER_CALL_FAILED,
            format!("cannot refresh the models.dev catalog from {url}: {why}"),
        )
    };
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| failed(e.to_string()))?;
    if !response.status().is_success() {
        return Err(failed(format!("HTTP {}", response.status().as_u16())));
    }
    let text = response.text().await.map_err(|e| failed(e.to_string()))?;
    let doc: Value = serde_json::from_str(&text).map_err(|e| failed(e.to_string()))?;
    let snapshot = ModelsDev::parse(&doc);
    if snapshot.windows.is_empty() {
        return Err(failed("no model has a context limit".to_owned()));
    }
    Ok(snapshot)
}

/// Caches `snapshot` under `dir` (owner-only, atomic).
///
/// # Errors
///
/// `home.io`.
///
/// # Panics
///
/// Never: the document holds only strings and numbers.
pub fn write(dir: &Path, snapshot: &ModelsDev) -> Result<(), Error> {
    let doc: serde_json::Map<String, Value> = snapshot
        .windows
        .iter()
        .map(|(provider, models)| {
            let models: serde_json::Map<String, Value> = models
                .iter()
                .map(|(id, w)| {
                    (
                        id.clone(),
                        serde_json::json!({"id": id, "limit": {"context": w}}),
                    )
                })
                .collect();
            (
                provider.clone(),
                serde_json::json!({"id": provider, "models": models}),
            )
        })
        .collect();
    let io = |e: std::io::Error| {
        Error::new(
            &codes::HOME_IO,
            format!(
                "cannot cache the models.dev catalog in {}: {e}",
                dir.display()
            ),
        )
    };
    pmt_core::private_fs::create_dir(dir).map_err(io)?;
    let text = serde_json::to_vec(&Value::Object(doc)).expect("JSON values serialize");
    pmt_core::private_fs::write_atomic(&dir.join(CACHE_FILE), &text).map_err(io)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_snapshot_maps_provider_ids_and_falls_back_by_model() {
        let snapshot = ModelsDev::load(None);
        assert_eq!(
            snapshot.context_window("openai-api", "gpt-4o"),
            Some(128_000)
        );
        assert_eq!(
            snapshot.context_window("gemini", "gemini-2.5-pro"),
            Some(1_048_576)
        );
        // OpenRouter-style ids find the vendor's entry.
        assert_eq!(
            snapshot.context_window("somerouter", "openai/gpt-4o"),
            Some(128_000)
        );
        assert_eq!(snapshot.context_window("openai-api", "nope"), None);
    }
}

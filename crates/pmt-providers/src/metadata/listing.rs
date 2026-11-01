//! Live model listings (`GET <models_url>`), cached per provider in the
//! profile's `cache/models/<provider>.json` for
//! `providers.<id>.catalog_ttl_secs`.

use std::path::{Path, PathBuf};

use jiff::Timestamp;
use pmt_obs::usage::Price;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One listed model with whatever metadata the provider reports.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Listed {
    /// Model id.
    pub id: String,
    /// Context window, when reported.
    pub context_window: Option<u64>,
    /// Output cap, when reported.
    pub max_output_tokens: Option<u64>,
    /// Tool support, when reported.
    pub supports_tools: Option<bool>,
    /// Image input, when reported.
    pub supports_vision: Option<bool>,
    /// Per-million prices `[input, output, cache_read, cache_write]`.
    pub price: Option<[f64; 4]>,
}

impl Listed {
    /// The price as the usage ledger reads it.
    #[must_use]
    pub fn price(&self) -> Option<Price> {
        self.price
            .map(|[input, output, cache_read, cache_write]| Price {
                input,
                output,
                cache_read,
                cache_write,
                reasoning: None,
            })
    }
}

/// A provider's cached listing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Listing {
    /// When it was fetched (Unix seconds).
    pub fetched_at: i64,
    /// The models.
    pub models: Vec<Listed>,
}

impl Listing {
    /// Whether it is younger than `ttl_secs` at `now`.
    #[must_use]
    pub fn is_fresh(&self, ttl_secs: u64, now: Timestamp) -> bool {
        let age = now.as_second().saturating_sub(self.fetched_at);
        u64::try_from(age).is_ok_and(|age| age < ttl_secs)
    }
}

/// The cache file of `provider` under `dir`.
#[must_use]
pub fn cache_file(dir: &Path, provider: &str) -> PathBuf {
    dir.join(format!("{provider}.json"))
}

/// Reads a cached listing (absent or unreadable: `None`).
#[must_use]
pub fn read(dir: &Path, provider: &str) -> Option<Listing> {
    let text = std::fs::read_to_string(cache_file(dir, provider)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Writes a listing atomically (owner-only).
///
/// # Errors
///
/// I/O errors.
pub fn write(dir: &Path, provider: &str, listing: &Listing) -> std::io::Result<()> {
    pmt_core::private_fs::create_dir(dir)?;
    let text = serde_json::to_vec_pretty(listing).map_err(std::io::Error::other)?;
    pmt_core::private_fs::write_atomic(&cache_file(dir, provider), &text)
}

/// Parses an OpenAI-style `{"data": [...]}` (or bare array) listing,
/// reading `OpenRouter`, vLLM and llama.cpp metadata fields when present.
#[must_use]
pub fn parse(body: &Value) -> Vec<Listed> {
    let items = body
        .get("data")
        .or_else(|| body.get("models"))
        .unwrap_or(body)
        .as_array()
        .cloned()
        .unwrap_or_default();
    items.iter().filter_map(parse_model).collect()
}

fn parse_model(item: &Value) -> Option<Listed> {
    let id = item
        .get("id")
        .or_else(|| item.get("name"))?
        .as_str()?
        .to_owned();
    let number = |keys: &[&str]| {
        keys.iter()
            .find_map(|k| item.pointer(k).and_then(Value::as_u64))
    };
    let modalities = item
        .pointer("/architecture/input_modalities")
        .and_then(Value::as_array);
    let parameters = item.get("supported_parameters").and_then(Value::as_array);
    Some(Listed {
        id,
        context_window: number(&[
            "/context_length",
            "/context_window",
            "/max_model_len",
            "/meta/n_ctx_train",
        ]),
        max_output_tokens: number(&["/top_provider/max_completion_tokens", "/max_output_tokens"]),
        supports_tools: parameters.map(|p| p.iter().any(|v| v == "tools")),
        supports_vision: modalities.map(|m| m.iter().any(|v| v == "image")),
        price: price(item.get("pricing")),
    })
}

/// `OpenRouter` prices are USD per token as strings.
fn price(pricing: Option<&Value>) -> Option<[f64; 4]> {
    let pricing = pricing?;
    let per_million = |key: &str| {
        let v = pricing.get(key)?;
        let n = v.as_f64().or_else(|| v.as_str()?.parse().ok())?;
        Some(n * 1_000_000.0)
    };
    Some([
        per_million("prompt")?,
        per_million("completion")?,
        per_million("input_cache_read").unwrap_or(0.0),
        per_million("input_cache_write").unwrap_or(0.0),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openrouter_fields_are_read() {
        let body = serde_json::json!({"data": [{
            "id": "a/b", "context_length": 32768,
            "top_provider": {"max_completion_tokens": 4096},
            "architecture": {"input_modalities": ["text", "image"]},
            "supported_parameters": ["tools", "temperature"],
            "pricing": {"prompt": "0.000003", "completion": "0.000015"}
        }, {"id": "plain"}]});
        let listed = parse(&body);
        assert_eq!(listed[0].context_window, Some(32_768));
        assert_eq!(listed[0].supports_vision, Some(true));
        assert_eq!(listed[0].supports_tools, Some(true));
        let price = listed[0].price.unwrap();
        assert!((price[0] - 3.0).abs() < 1e-9 && (price[1] - 15.0).abs() < 1e-9);
        assert_eq!(
            listed[1],
            Listed {
                id: "plain".into(),
                ..Listed::default()
            }
        );
    }
}

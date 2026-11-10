//! The curated model catalog (spec: local-models, Hardware-priced model
//! catalog): `catalog.json` in this crate, compiled in. Each entry pins its
//! Hugging Face repository at a commit and every file's size and SHA-256,
//! and carries what pricing needs: the model's native window, the KV cache
//! bytes per token, active parameters (speed) and a quality rank. Reading
//! it sends no request.

mod price;

use pmt_core::error::{Error, codes};
use serde::{Deserialize, Serialize};

pub use price::{Fit, MIN_BITS, MIN_WINDOW, Priced, SPEED_FLOOR, price};

const EMBEDDED: &str = include_str!("../../catalog.json");
/// `schema` of the catalog.
pub const SCHEMA: &str = "prometheus.local-catalog.v1";

/// One pinned file of a repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoFile {
    /// Path in the repository.
    pub path: String,
    /// Size in bytes.
    pub size: u64,
    /// Lower-case hex SHA-256.
    pub sha256: String,
}

/// One quantisation of a model: its weight files (split GGUF parts in
/// order).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Quant {
    /// Name (`Q8_0`, `Q4_K_M`, `MXFP4`).
    pub quant: String,
    /// Bits per weight.
    pub bits: f64,
    /// The weight files.
    pub files: Vec<RepoFile>,
}

impl Quant {
    /// Bytes of the weights.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }
}

/// One catalog model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    /// Library id (`prometheus local pull <id>`, `llamacpp/<id>`).
    pub id: String,
    /// Display name.
    pub name: String,
    /// Hugging Face repository.
    pub repo: String,
    /// The pinned commit.
    pub revision: String,
    /// Parameters in billions.
    pub params_b: f64,
    /// Parameters active per token in billions (all of a dense model).
    pub active_params_b: f64,
    /// The model's native window.
    pub context_max: u64,
    /// KV cache bytes per token of context (f16, attention layers only).
    pub kv_bytes_per_token: u64,
    /// Rank among catalog models; higher is better.
    pub quality: u32,
    /// The distribution's default model (recommended whenever it fits).
    #[serde(default)]
    pub vendor_default: bool,
    /// Quantisations, any order.
    pub quants: Vec<Quant>,
    /// The vision projector, for models that see images.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projector: Option<RepoFile>,
}

/// The catalog.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Catalog {
    /// [`SCHEMA`].
    pub schema: String,
    /// The hub files are downloaded from.
    pub hub: String,
    /// The models.
    pub models: Vec<Entry>,
}

impl Catalog {
    /// The catalog compiled into this binary.
    ///
    /// # Panics
    ///
    /// Never for the committed file (unit-tested).
    #[must_use]
    pub fn embedded() -> Self {
        Self::from_json(EMBEDDED).expect("the embedded catalog parses")
    }

    /// A catalog from JSON (fixtures).
    ///
    /// # Errors
    ///
    /// The parse error.
    pub fn from_json(text: &str) -> Result<Self, String> {
        serde_json::from_str(text).map_err(|e| e.to_string())
    }

    /// The entry `id`.
    ///
    /// # Errors
    ///
    /// `local.not_found` (66).
    pub fn entry(&self, id: &str) -> Result<&Entry, Error> {
        self.models.iter().find(|m| m.id == id).ok_or_else(|| {
            Error::new(
                &codes::LOCAL_NOT_FOUND,
                format!("'{id}' is not in the local model catalog"),
            )
            .with_hint("prometheus local catalog lists it; prometheus local search <query> searches Hugging Face")
        })
    }

    /// The download URL of `file` of `entry`.
    #[must_use]
    pub fn url(&self, entry: &Entry, file: &RepoFile) -> String {
        format!(
            "{}/{}/resolve/{}/{}",
            self.hub.trim_end_matches('/'),
            entry.repo,
            entry.revision,
            file.path
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{Catalog, MIN_BITS, SCHEMA};

    #[test]
    fn the_embedded_catalog_pins_every_file_and_no_build_below_4_bits() {
        let catalog = Catalog::embedded();
        assert_eq!(catalog.schema, SCHEMA);
        assert!(catalog.hub.starts_with("https://"));
        for entry in &catalog.models {
            assert_eq!(entry.revision.len(), 40, "{}", entry.id);
            assert!(!entry.quants.is_empty(), "{}", entry.id);
            for quant in &entry.quants {
                assert!(quant.bits >= MIN_BITS, "{} {}", entry.id, quant.quant);
                for file in &quant.files {
                    assert_eq!(file.sha256.len(), 64, "{}", file.path);
                    assert!(file.size > 0, "{}", file.path);
                }
            }
        }
        let ids: std::collections::BTreeSet<_> = catalog.models.iter().map(|m| &m.id).collect();
        assert_eq!(ids.len(), catalog.models.len(), "ids are unique");
    }
}

//! Ranking archival entries for recall (spec: memory, Semantic recall).
//!
//! Without `memory.recall.embedding_model` entries are ranked lexically by
//! FTS5 BM25 (an entry sharing no query word is never recalled). With one,
//! the score is `0.6 * cosine + 0.4 * normalized BM25` (BM25 divided by
//! the best hit's), entries below `memory.recall.min_score` are dropped,
//! and an entry with no shared word can still be recalled. Embeddings are
//! computed lazily and stored per (entry, model, content hash); text bound
//! for a remote embedding model is redacted first. An embedding failure
//! degrades to lexical ranking with a WARN.

use std::collections::BTreeMap;

use jiff::Timestamp;
use pmt_agent::turn::model::BoxFuture;
use pmt_config::schema::MemoryConfig;
use pmt_core::error::Error;
use pmt_core::secret::Redactor;
use pmt_store::memory_index::{Embedding, content_sha};
use pmt_store::{MemoryEntry, MemoryStatus, MemoryTier, Store};
use serde::Serialize;

use crate::{budget, snapshot};

/// Weight of the cosine similarity in the hybrid score.
pub const COSINE_WEIGHT: f64 = 0.6;
/// Weight of the normalized BM25 score in the hybrid score.
pub const BM25_WEIGHT: f64 = 0.4;
/// Entries embedded per request.
const BATCH: usize = 64;

/// Computes embeddings for semantic recall.
pub trait Embedder: Send + Sync {
    /// The model id embeddings are stored under.
    fn model(&self) -> String;
    /// Whether text leaves the machine (redacted first).
    fn remote(&self) -> bool;
    /// One vector per input, in order.
    fn embed(&self, inputs: Vec<String>) -> BoxFuture<'_, Result<Vec<Vec<f32>>, Error>>;
}

/// One ranked entry and how its score was made.
#[derive(Debug, Clone, Serialize)]
pub struct Scored {
    /// The entry.
    #[serde(skip)]
    pub entry: MemoryEntry,
    /// Its id.
    pub id: String,
    /// BM25 relevance divided by the best hit's (0 without a shared word).
    pub bm25: f64,
    /// Cosine similarity to the query (hybrid only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cosine: Option<f64>,
    /// The ranking score.
    pub score: f64,
}

/// The query words handed to FTS5 (stop words and short words dropped;
/// FTS5's porter tokenizer stems both sides).
fn words(query: &str) -> Vec<String> {
    let mut out: Vec<String> = query
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 3 && !crate::recall::is_stop_word(w))
        .map(str::to_owned)
        .collect();
    out.sort();
    out.dedup();
    out
}

/// The archival entries recall may return.
fn eligible(store: &Store, config: &MemoryConfig) -> Result<Vec<MemoryEntry>, Error> {
    let targets = budget::enabled(config);
    let now = Timestamp::now();
    Ok(store
        .memory_entries(None)?
        .into_iter()
        .filter(|e| {
            e.tier == MemoryTier::Archival
                && e.status != MemoryStatus::Dormant
                && targets.contains(&e.target)
                && !snapshot::expired(e, now)
        })
        .collect())
}

fn cosine(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let (mut dot, mut na, mut nb) = (0.0_f64, 0.0_f64, 0.0_f64);
    for (x, y) in a.iter().zip(b) {
        let (x, y) = (f64::from(*x), f64::from(*y));
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na.sqrt() * nb.sqrt())
    }
}

/// Ranks `query` against the eligible entries, best first.
///
/// # Errors
///
/// Store errors (an embedding failure is not one: it degrades).
pub async fn rank(
    db: &Store,
    config: &MemoryConfig,
    embedder: Option<&dyn Embedder>,
    redactor: &Redactor,
    query: &str,
) -> Result<Vec<Scored>, Error> {
    let entries = eligible(db, config)?;
    let hits: BTreeMap<String, f64> = db.memory_bm25(&words(query))?.into_iter().collect();
    let best = hits.values().copied().fold(0.0_f64, f64::max);
    let bm25 = |id: &str| {
        hits.get(id)
            .filter(|_| best > 0.0)
            .map_or(0.0, |s| s / best)
    };
    let semantic = match embedder {
        Some(embedder) => match similarities(db, embedder, redactor, &entries, query).await {
            Ok(found) => Some(found),
            Err(err) => {
                tracing::warn!("semantic recall unavailable, ranking lexically: {err}");
                None
            }
        },
        None => None,
    };
    let mut scored: Vec<Scored> = entries
        .into_iter()
        .filter_map(|entry| {
            let lexical = bm25(&entry.id);
            let (cosine, score) = match &semantic {
                Some(found) => {
                    let cos = found.get(&entry.id).copied().unwrap_or(0.0);
                    let score = COSINE_WEIGHT * cos + BM25_WEIGHT * lexical;
                    if score < config.recall.min_score {
                        return None;
                    }
                    (Some(cos), score)
                }
                None if hits.contains_key(&entry.id) => (None, lexical),
                None => return None,
            };
            Some(Scored {
                id: entry.id.clone(),
                entry,
                bm25: lexical,
                cosine,
                score,
            })
        })
        .collect();
    scored.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then(b.entry.updated_at.cmp(&a.entry.updated_at))
    });
    Ok(scored)
}

/// The cosine similarity of every entry to `query`, embedding entries
/// whose stored vector is missing or stale first.
async fn similarities(
    store: &Store,
    embedder: &dyn Embedder,
    redactor: &Redactor,
    entries: &[MemoryEntry],
    query: &str,
) -> Result<BTreeMap<String, f64>, Error> {
    let model = embedder.model();
    let outbound = |text: &str| {
        if embedder.remote() {
            redactor.redact_all(text)
        } else {
            text.to_owned()
        }
    };
    let mut stored = store.memory_embeddings(&model)?;
    let stale: Vec<&MemoryEntry> = entries
        .iter()
        .filter(|e| {
            stored
                .get(&e.id)
                .is_none_or(|s| s.content_sha != content_sha(&e.content))
        })
        .collect();
    for chunk in stale.chunks(BATCH) {
        let inputs = chunk.iter().map(|e| outbound(&e.content)).collect();
        let vectors = embedder.embed(inputs).await?;
        let fresh: Vec<(String, Embedding)> = chunk
            .iter()
            .zip(vectors)
            .map(|(e, vector)| {
                let embedding = Embedding {
                    content_sha: content_sha(&e.content),
                    vector,
                };
                (e.id.clone(), embedding)
            })
            .collect();
        store.put_memory_embeddings(&model, &fresh)?;
        stored.extend(fresh);
    }
    let query = embedder
        .embed(vec![outbound(query)])
        .await?
        .pop()
        .unwrap_or_default();
    Ok(entries
        .iter()
        .filter_map(|e| {
            stored
                .get(&e.id)
                .map(|s| (e.id.clone(), cosine(&query, &s.vector)))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_is_one_for_parallel_and_zero_for_orthogonal_vectors() {
        assert!((cosine(&[1.0, 2.0], &[2.0, 4.0]) - 1.0).abs() < 1e-9);
        assert!(cosine(&[1.0, 0.0], &[0.0, 3.0]).abs() < 1e-9);
        assert!(cosine(&[], &[]).abs() < 1e-9);
    }

    #[test]
    fn words_drop_stop_words_and_duplicates() {
        assert_eq!(words("Which DB does Atlas use? atlas"), ["atlas", "use"]);
    }
}

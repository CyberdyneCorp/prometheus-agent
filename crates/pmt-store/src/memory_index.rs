//! Derived memory indexes (spec: memory, Semantic recall): the FTS5 table
//! lexical recall ranks with BM25, and the local embeddings semantic recall
//! compares. Both are kept in step by [`Store::memory_batch`]; embeddings
//! are filled lazily by recall, keyed by entry, model and content hash.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use pmt_core::error::Error;
use rusqlite::{Transaction, params};
use sha2::{Digest, Sha256};

use crate::error::sql;
use crate::memory::MemoryEntry;
use crate::store::Store;

/// One stored embedding.
#[derive(Debug, Clone, PartialEq)]
pub struct Embedding {
    /// The sha256 (hex) of the content it was computed from.
    pub content_sha: String,
    /// The vector.
    pub vector: Vec<f32>,
}

/// The hash embeddings are keyed by.
#[must_use]
pub fn content_sha(content: &str) -> String {
    Sha256::digest(content.as_bytes())
        .iter()
        .fold(String::with_capacity(64), |mut hex, b| {
            let _ = write!(hex, "{b:02x}");
            hex
        })
}

/// Updates the indexes for what changed between `before` and `after`.
pub(crate) fn reindex(
    tx: &Transaction<'_>,
    before: &[MemoryEntry],
    after: &[MemoryEntry],
) -> Result<(), Error> {
    let old: BTreeMap<&str, &str> = before
        .iter()
        .map(|e| (e.id.as_str(), e.content.as_str()))
        .collect();
    let new: BTreeMap<&str, &str> = after
        .iter()
        .map(|e| (e.id.as_str(), e.content.as_str()))
        .collect();
    for id in old.keys().filter(|id| !new.contains_key(*id)) {
        tx.execute("DELETE FROM memory_fts WHERE entry_id = ?1", [id])
            .map_err(sql)?;
        tx.execute("DELETE FROM memory_embeddings WHERE entry_id = ?1", [id])
            .map_err(sql)?;
    }
    for (id, content) in &new {
        if old.get(id) == Some(content) {
            continue;
        }
        tx.execute("DELETE FROM memory_fts WHERE entry_id = ?1", [id])
            .map_err(sql)?;
        tx.execute(
            "INSERT INTO memory_fts (content, entry_id) VALUES (?1, ?2)",
            [content, id],
        )
        .map_err(sql)?;
    }
    Ok(())
}

/// An FTS5 query matching any of `words` (each quoted, so punctuation and
/// operators in user text are literal).
fn match_any(words: &[String]) -> String {
    words
        .iter()
        .map(|w| format!("\"{}\"", w.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" OR ")
}

fn encode(vector: &[f32]) -> Vec<u8> {
    vector.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn decode(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

impl Store {
    /// Entries matching any of `words`, with their BM25 relevance (higher
    /// is better; FTS5's `bm25()` negated), best first.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn memory_bm25(&self, words: &[String]) -> Result<Vec<(String, f64)>, Error> {
        if words.is_empty() {
            return Ok(Vec::new());
        }
        let query = match_any(words);
        self.read(|conn| {
            let mut stmt = conn
                .prepare_cached(
                    "SELECT entry_id, -bm25(memory_fts) FROM memory_fts \
                     WHERE memory_fts MATCH ?1 ORDER BY bm25(memory_fts)",
                )
                .map_err(sql)?;
            let rows = stmt
                .query_map([query], |row| Ok((row.get(0)?, row.get(1)?)))
                .map_err(sql)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(sql)
        })
    }

    /// The stored embeddings of `model`, by entry id.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn memory_embeddings(&self, model: &str) -> Result<BTreeMap<String, Embedding>, Error> {
        self.read(|conn| {
            let mut stmt = conn
                .prepare_cached(
                    "SELECT entry_id, content_sha, vector FROM memory_embeddings WHERE model = ?1",
                )
                .map_err(sql)?;
            let rows = stmt
                .query_map([model], |row| {
                    let bytes: Vec<u8> = row.get(2)?;
                    Ok((
                        row.get::<_, String>(0)?,
                        Embedding {
                            content_sha: row.get(1)?,
                            vector: decode(&bytes),
                        },
                    ))
                })
                .map_err(sql)?;
            rows.collect::<Result<BTreeMap<_, _>, _>>().map_err(sql)
        })
    }

    /// Stores (replacing) embeddings of `model` by entry id.
    ///
    /// # Errors
    ///
    /// `store.sqlite`, `store.busy`.
    pub fn put_memory_embeddings(
        &self,
        model: &str,
        embeddings: &[(String, Embedding)],
    ) -> Result<(), Error> {
        if embeddings.is_empty() {
            return Ok(());
        }
        self.write(|tx| {
            for (id, embedding) in embeddings {
                tx.execute(
                    "INSERT INTO memory_embeddings (entry_id, model, content_sha, vector) \
                     VALUES (?1, ?2, ?3, ?4) ON CONFLICT (entry_id, model) DO UPDATE SET \
                     content_sha = excluded.content_sha, vector = excluded.vector",
                    params![id, model, embedding.content_sha, encode(&embedding.vector)],
                )
                .map_err(sql)?;
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vectors_round_trip_and_queries_quote_words() {
        let v = vec![0.5_f32, -1.25, 3.0];
        assert_eq!(decode(&encode(&v)), v);
        assert_eq!(
            match_any(&["db".into(), "a\"b".into()]),
            "\"db\" OR \"a\"\"b\""
        );
    }
}

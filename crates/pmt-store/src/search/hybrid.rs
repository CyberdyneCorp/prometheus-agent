//! Hybrid semantic + full-text search (spec: session-storage, Hybrid
//! semantic and full-text search).
//!
//! `message_embeddings` is a derived index like the word index: one vector
//! per active user or assistant message and embedding model, filled in the
//! background by the host ([`Store::unembedded_messages`],
//! [`Store::put_message_embeddings`]), never inside a turn. A hybrid search
//! fuses the full-text ranking and the cosine ranking of the query's vector
//! with reciprocal rank fusion (`k = 60`); without a query vector, or when
//! the model has no stored vectors, it answers full-text only, marked
//! [`SEMANTIC_UNAVAILABLE`].

use std::collections::BTreeMap;

use pmt_core::error::Error;
use pmt_core::id::{MessageId, SessionId};
use rusqlite::types::Value;
use rusqlite::{Connection, params, params_from_iter};

use super::{Filters, HIT_COLUMNS, SearchHit, SearchOptions, SearchResults, SearchSort, content};
use crate::error::sql;
use crate::memory_index::{Embedding, content_sha};
use crate::store::Store;

/// The reciprocal rank fusion constant.
pub const RRF_K: f64 = 60.0;
/// Candidates taken from each ranking before fusion.
pub const CANDIDATES: u32 = 100;
/// The `degraded` marker of a hybrid search answered by full text alone.
pub const SEMANTIC_UNAVAILABLE: &str = "semantic_unavailable";

/// A query's vector and the model that computed it.
#[derive(Debug, Clone, Copy)]
pub struct QueryVector<'a> {
    /// The embedding model (stored vectors of other models are ignored).
    pub model: &'a str,
    /// The vector.
    pub vector: &'a [f32],
}

/// A message waiting for its embedding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unembedded {
    /// The message.
    pub message_id: MessageId,
    /// Its text.
    pub text: String,
}

/// The fused order of `rankings` (each best first): highest
/// `sum(1 / (k + rank))` first, ties by the full-text order.
#[must_use]
pub fn fuse(rankings: &[Vec<MessageId>]) -> Vec<(MessageId, f64)> {
    let mut scores: BTreeMap<MessageId, (f64, usize)> = BTreeMap::new();
    let mut order = 0usize;
    for ranking in rankings {
        for (rank, id) in ranking.iter().enumerate() {
            let entry = scores.entry(*id).or_insert_with(|| {
                order += 1;
                (0.0, order)
            });
            // Ranks are small (at most CANDIDATES): exact in f64.
            #[allow(clippy::cast_precision_loss)]
            let r = (rank + 1) as f64;
            entry.0 += 1.0 / (RRF_K + r);
        }
    }
    let mut fused: Vec<(MessageId, f64, usize)> = scores
        .into_iter()
        .map(|(id, (score, first))| (id, score, first))
        .collect();
    fused.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.2.cmp(&b.2)));
    fused
        .into_iter()
        .map(|(id, score, _)| (id, score))
        .collect()
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

fn decode(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

fn encode(vector: &[f32]) -> Vec<u8> {
    vector.iter().flat_map(|x| x.to_le_bytes()).collect()
}

/// The messages passing `filters` that have a vector of `query.model`
/// with a positive cosine, best first (at most [`CANDIDATES`]).
fn semantic(
    conn: &Connection,
    filters: &Filters,
    query: QueryVector<'_>,
) -> Result<Vec<MessageId>, Error> {
    let sql_text = format!(
        "SELECT m.id, e.vector FROM message_embeddings e JOIN messages m ON m.id = e.message_id \
         JOIN sessions s ON s.id = m.session_id WHERE e.model = ?{}",
        filters.clause
    );
    let mut values = vec![Value::Text(query.model.to_owned())];
    values.extend(filters.params.iter().cloned());
    let mut stmt = conn.prepare(&sql_text).map_err(sql)?;
    let mut rows = stmt.query(params_from_iter(values)).map_err(sql)?;
    let mut scored = Vec::new();
    while let Some(row) = rows.next().map_err(sql)? {
        let id = MessageId::parse(&row.get::<_, String>(0).map_err(sql)?)?;
        let bytes: Vec<u8> = row.get(1).map_err(sql)?;
        let similarity = cosine(query.vector, &decode(&bytes));
        // Unrelated or opposite meaning is no semantic match at all.
        if similarity > 0.0 {
            scored.push((id, similarity));
        }
    }
    scored.sort_by(|a, b| b.1.total_cmp(&a.1));
    scored.truncate(usize::try_from(CANDIDATES).unwrap_or(usize::MAX));
    Ok(scored.into_iter().map(|(id, _)| id).collect())
}

/// A hit for a message only the vector ranking found: its leading text.
fn plain_hit(
    conn: &Connection,
    blob_dir: &crate::blobs::Blobs,
    id: MessageId,
) -> Result<Option<SearchHit>, Error> {
    let sql_text = format!(
        "SELECT {HIT_COLUMNS}, NULL AS score FROM messages m JOIN sessions s ON s.id = m.session_id \
         WHERE m.id = ?1"
    );
    let mut stmt = conn.prepare_cached(&sql_text).map_err(sql)?;
    let mut rows = stmt.query([id.to_string()]).map_err(sql)?;
    let Some(row) = rows.next().map_err(sql)? else {
        return Ok(None);
    };
    let text = content(row, blob_dir)?;
    Ok(Some(SearchHit {
        session_id: SessionId::parse(&row.get::<_, String>(0).map_err(sql)?)?,
        session_title: row.get(1).map_err(sql)?,
        message_id: id,
        role: row.get::<_, String>(3).map_err(sql)?.parse()?,
        created_at: jiff::Timestamp::from_millisecond(row.get(4).map_err(sql)?)
            .unwrap_or(jiff::Timestamp::UNIX_EPOCH),
        snippet: super::snippet(&text, &[]),
        score: None,
    }))
}

impl Store {
    /// `search(..., mode = "hybrid")`: the full-text and `vector`
    /// rankings fused by reciprocal rank fusion, in fused order (`sort` is
    /// ignored), each hit's `score` its fused score. Without `vector`, or
    /// when no message has a vector of its model, the full-text results
    /// with `degraded = Some(`[`SEMANTIC_UNAVAILABLE`]`)`.
    ///
    /// # Errors
    ///
    /// As [`Store::search`].
    pub fn hybrid_search(
        &self,
        raw_query: &str,
        options: &SearchOptions,
        vector: Option<QueryVector<'_>>,
    ) -> Result<SearchResults, Error> {
        let query = match vector {
            Some(query) if self.has_message_embeddings(query.model)? => query,
            _ => {
                let mut degraded = self.search(raw_query, options)?;
                degraded.degraded = Some(SEMANTIC_UNAVAILABLE);
                return Ok(degraded);
            }
        };
        let candidates = SearchOptions {
            limit: CANDIDATES,
            offset: 0,
            sort: SearchSort::Rank,
            ..options.clone()
        };
        let text = self.search(raw_query, &candidates)?;
        let blob_dir = self.blobs();
        let filters = Filters::of(options);
        let semantic = self.read(|conn| semantic(conn, &filters, query))?;
        let lexical: Vec<MessageId> = text.hits.iter().map(|h| h.message_id).collect();
        let mut by_id: BTreeMap<MessageId, SearchHit> =
            text.hits.into_iter().map(|h| (h.message_id, h)).collect();
        let skip = usize::try_from(options.offset).unwrap_or(usize::MAX);
        let take = usize::try_from(options.limit).unwrap_or(usize::MAX);
        let mut hits = Vec::new();
        for (id, score) in fuse(&[lexical, semantic]).into_iter().skip(skip).take(take) {
            let hit = match by_id.remove(&id) {
                Some(hit) => Some(hit),
                None => self.read(|conn| plain_hit(conn, &blob_dir, id))?,
            };
            if let Some(mut hit) = hit {
                hit.score = Some(score);
                hits.push(hit);
            }
        }
        Ok(SearchResults {
            hits,
            mode: text.mode,
            degraded: None,
        })
    }

    /// Whether any message has a vector of `model`.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn has_message_embeddings(&self, model: &str) -> Result<bool, Error> {
        self.read(|conn| {
            conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM message_embeddings WHERE model = ?1)",
                [model],
                |r| r.get(0),
            )
            .map_err(sql)
        })
    }

    /// Up to `limit` active user and assistant messages with text and no
    /// vector of `model` yet, oldest first.
    ///
    /// # Errors
    ///
    /// `store.sqlite`, blob errors.
    pub fn unembedded_messages(&self, model: &str, limit: u32) -> Result<Vec<Unembedded>, Error> {
        let blob_dir = self.blobs();
        self.read(|conn| {
            let sql_text = format!(
                "SELECT {HIT_COLUMNS} FROM messages m JOIN sessions s ON s.id = m.session_id \
                 WHERE m.active = 1 AND m.role IN ('user', 'assistant') \
                 AND (m.content IS NOT NULL OR m.content_ref IS NOT NULL) \
                 AND NOT EXISTS (SELECT 1 FROM message_embeddings e \
                     WHERE e.message_id = m.id AND e.model = ?1) \
                 ORDER BY m.created_at, m.rowid LIMIT ?2"
            );
            let mut stmt = conn.prepare(&sql_text).map_err(sql)?;
            let mut rows = stmt.query(params![model, limit]).map_err(sql)?;
            let mut out = Vec::new();
            while let Some(row) = rows.next().map_err(sql)? {
                let text = content(row, &blob_dir)?;
                if text.trim().is_empty() {
                    continue;
                }
                out.push(Unembedded {
                    message_id: MessageId::parse(&row.get::<_, String>(2).map_err(sql)?)?,
                    text,
                });
            }
            Ok(out)
        })
    }

    /// Stores vectors of `model`; vectors of deleted messages are dropped.
    ///
    /// # Errors
    ///
    /// `store.sqlite`, `store.busy`.
    pub fn put_message_embeddings(
        &self,
        model: &str,
        vectors: &[(MessageId, String, Vec<f32>)],
    ) -> Result<(), Error> {
        if vectors.is_empty() {
            return Ok(());
        }
        self.write(|tx| {
            for (id, text, vector) in vectors {
                let embedding = Embedding {
                    content_sha: content_sha(text),
                    vector: vector.clone(),
                };
                tx.execute(
                    "INSERT INTO message_embeddings (message_id, model, content_sha, vector) \
                     SELECT ?1, ?2, ?3, ?4 WHERE EXISTS (SELECT 1 FROM messages WHERE id = ?1) \
                     ON CONFLICT (message_id, model) DO UPDATE SET \
                     content_sha = excluded.content_sha, vector = excluded.vector",
                    params![
                        id.to_string(),
                        model,
                        embedding.content_sha,
                        encode(&embedding.vector)
                    ],
                )
                .map_err(sql)?;
            }
            tx.execute(
                "DELETE FROM message_embeddings WHERE message_id NOT IN (SELECT id FROM messages)",
                [],
            )
            .map_err(sql)?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(n: usize) -> Vec<MessageId> {
        (0..n).map(|_| MessageId::new()).collect()
    }

    #[test]
    fn fusion_rewards_agreement_and_keeps_single_list_hits() {
        let m = ids(4);
        // Lexical: a, b, c. Semantic: d, b.
        let fused = fuse(&[vec![m[0], m[1], m[2]], vec![m[3], m[1]]]);
        let order: Vec<MessageId> = fused.iter().map(|(id, _)| *id).collect();
        assert_eq!(order, [m[1], m[0], m[3], m[2]]);
        let expected = 1.0 / 62.0 + 1.0 / 62.0;
        assert!((fused[0].1 - expected).abs() < 1e-12);
    }

    #[test]
    fn cosine_ranks_parallel_vectors_first() {
        assert!((cosine(&[1.0, 0.0], &[2.0, 0.0]) - 1.0).abs() < 1e-12);
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-12);
    }
}

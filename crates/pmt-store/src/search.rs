//! Full-text session search (spec: session-storage, Full-text session
//! search; Search query sanitisation; Derived indexes fail open).
//!
//! A sanitised query goes to the word index (BM25 ranked), or to the
//! trigram index when it contains CJK text or a term under 3 characters
//! (terms under 3 characters match by substring there). When the indexes
//! are stale or fail, the same filters run over the canonical rows instead
//! ([`SearchMode::Scan`]). Rewound rows are left out unless asked for;
//! compaction-archived rows are always searchable.

mod query;

use jiff::Timestamp;
use pmt_core::error::Error;
use pmt_core::id::{MessageId, SessionId};
use rusqlite::types::Value;
use rusqlite::{Connection, Row, params_from_iter};

pub use query::MAX_QUERY_CHARS;
use query::Query;

use crate::blobs::{self, BlobRef};
use crate::error::{corrupt, sql};
use crate::index::{self, TRIGRAM_INDEX, WORD_INDEX};
use crate::model::{Role, Source};
use crate::store::Store;

/// Longest snippet, markers and ellipses included.
pub const SNIPPET_CHARS: usize = 200;

/// Marks the start and end of a match in a snippet.
pub const MATCH_START: char = '«';
/// See [`MATCH_START`].
pub const MATCH_END: char = '»';

/// Result order.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SearchSort {
    /// Best BM25 rank first (newest first when there is no rank).
    #[default]
    Rank,
    /// Newest message first.
    Newest,
    /// Oldest message first.
    Oldest,
}

/// Search filters and paging.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchOptions {
    /// Hits to return (default 20).
    pub limit: u32,
    /// Hits to skip (default 0).
    pub offset: u32,
    /// Result order.
    pub sort: SearchSort,
    /// Only these sources (empty: all).
    pub sources: Vec<Source>,
    /// Never these sources.
    pub exclude_sources: Vec<Source>,
    /// Only these roles (empty: all).
    pub roles: Vec<Role>,
    /// Messages created at or after this time.
    pub after: Option<Timestamp>,
    /// Messages created before this time.
    pub before: Option<Timestamp>,
    /// Only this session.
    pub session_id: Option<SessionId>,
    /// Never these sessions.
    pub exclude_session_ids: Vec<SessionId>,
    /// Only pinned (`Some(true)`) or unpinned (`Some(false)`) sessions.
    pub pinned: Option<bool>,
    /// Include archived sessions (default true).
    pub include_archived: bool,
    /// Include rewound messages (default false).
    pub include_inactive: bool,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            limit: 20,
            offset: 0,
            sort: SearchSort::Rank,
            sources: Vec::new(),
            exclude_sources: Vec::new(),
            roles: Vec::new(),
            after: None,
            before: None,
            session_id: None,
            exclude_session_ids: Vec::new(),
            pinned: None,
            include_archived: true,
            include_inactive: false,
        }
    }
}

/// Which path answered a search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchMode {
    /// The word index.
    Word,
    /// The trigram index.
    Trigram,
    /// A scan of the canonical rows (stale or failing indexes).
    Scan,
}

/// One matching message.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    /// Its session.
    pub session_id: SessionId,
    /// The session's title.
    pub session_title: Option<String>,
    /// The message.
    pub message_id: MessageId,
    /// Its role.
    pub role: Role,
    /// When it was stored.
    pub created_at: Timestamp,
    /// At most [`SNIPPET_CHARS`] characters around the first match, with
    /// matches between [`MATCH_START`] and [`MATCH_END`].
    pub snippet: String,
    /// BM25 score (lower is better), when ranked.
    pub score: Option<f64>,
}

/// A search's hits and the path that produced them.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchResults {
    /// The hits, in the requested order.
    pub hits: Vec<SearchHit>,
    /// The index (or scan) used.
    pub mode: SearchMode,
}

/// The SQL filter clause and its parameters.
struct Filters {
    clause: String,
    params: Vec<Value>,
}

fn list(values: &[&str]) -> (String, Vec<Value>) {
    let marks = vec!["?"; values.len()].join(", ");
    (
        marks,
        values
            .iter()
            .map(|v| Value::Text((*v).to_owned()))
            .collect(),
    )
}

impl Filters {
    fn of(options: &SearchOptions) -> Self {
        let mut filters = Self {
            clause: String::new(),
            params: Vec::new(),
        };
        let as_str = |s: &Source| s.as_str();
        filters.any_of(
            "s.source IN",
            &options.sources.iter().map(as_str).collect::<Vec<_>>(),
        );
        filters.any_of(
            "s.source NOT IN",
            &options
                .exclude_sources
                .iter()
                .map(as_str)
                .collect::<Vec<_>>(),
        );
        filters.any_of(
            "m.role IN",
            &options.roles.iter().map(|r| r.as_str()).collect::<Vec<_>>(),
        );
        let excluded: Vec<String> = options
            .exclude_session_ids
            .iter()
            .map(ToString::to_string)
            .collect();
        filters.any_of(
            "m.session_id NOT IN",
            &excluded.iter().map(String::as_str).collect::<Vec<_>>(),
        );
        if let Some(after) = options.after {
            filters.push("m.created_at >= ?", Value::Integer(after.as_millisecond()));
        }
        if let Some(before) = options.before {
            filters.push("m.created_at < ?", Value::Integer(before.as_millisecond()));
        }
        if let Some(session) = options.session_id {
            filters.push("m.session_id = ?", Value::Text(session.to_string()));
        }
        if let Some(pinned) = options.pinned {
            filters.push("s.pinned = ?", Value::Integer(i64::from(pinned)));
        }
        if !options.include_archived {
            filters.clause.push_str(" AND s.archived = 'none'");
        }
        if !options.include_inactive {
            filters
                .clause
                .push_str(" AND NOT (m.active = 0 AND m.compacted = 0)");
        }
        filters
    }

    fn any_of(&mut self, column_in: &str, values: &[&str]) {
        if values.is_empty() {
            return;
        }
        let (marks, params) = list(values);
        self.clause.push_str(" AND ");
        self.clause.push_str(column_in);
        self.clause.push_str(" (");
        self.clause.push_str(&marks);
        self.clause.push(')');
        self.params.extend(params);
    }

    fn push(&mut self, condition: &str, value: Value) {
        self.clause.push_str(" AND ");
        self.clause.push_str(condition);
        self.params.push(value);
    }
}

const HIT_COLUMNS: &str =
    "m.session_id, s.title, m.id, m.role, m.created_at, m.content, m.content_ref";

fn has_cjk(text: &str) -> bool {
    text.chars().any(|c| {
        matches!(c as u32,
            0x3040..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xAC00..=0xD7AF
            | 0xF900..=0xFAFF | 0xFF66..=0xFF9F | 0x20000..=0x2FFFF)
    })
}

impl Store {
    /// Searches the profile's messages for `raw_query`.
    ///
    /// # Errors
    ///
    /// `store.blob_missing` / `store.blob_corrupt` (a spilled hit), and
    /// `store.sqlite` only when the canonical scan itself fails: index
    /// failures fall back to the scan.
    pub fn search(&self, raw_query: &str, options: &SearchOptions) -> Result<SearchResults, Error> {
        let query = Query::parse(raw_query);
        if query.is_empty() || options.limit == 0 {
            return Ok(SearchResults {
                hits: Vec::new(),
                mode: SearchMode::Word,
            });
        }
        let blob_dir = self.location().blobs().to_path_buf();
        let filters = Filters::of(options);
        self.read(|conn| {
            if !index::is_stale(conn)? {
                let indexed = search_index(conn, &blob_dir, &query, &filters, options);
                match indexed {
                    Ok(results) => return Ok(results),
                    Err(e) => {
                        tracing::warn!("search index failed ({e}); scanning messages instead");
                    }
                }
            }
            scan(conn, &blob_dir, &query, &filters, options)
        })
    }
}

fn search_index(
    conn: &Connection,
    blob_dir: &std::path::Path,
    query: &Query,
    filters: &Filters,
    options: &SearchOptions,
) -> Result<SearchResults, Error> {
    let terms = query.terms();
    let trigram = terms.iter().any(|t| has_cjk(t) || t.chars().count() < 3);
    let (table, mode) = if trigram {
        (TRIGRAM_INDEX, SearchMode::Trigram)
    } else {
        (WORD_INDEX, SearchMode::Word)
    };
    let short = trigram && terms.iter().any(|t| t.chars().count() < 3);
    let mut params = Vec::new();
    let condition = if short {
        // The trigram index answers LIKE for terms of any length.
        let likes: Vec<String> = terms
            .iter()
            .map(|term| {
                params.push(Value::Text(format!("%{}%", escape_like(term))));
                format!("{table}.text LIKE ? ESCAPE '\\'")
            })
            .collect();
        likes.join(" AND ")
    } else {
        params.push(Value::Text(query.render(!trigram)));
        format!("{table} MATCH ?")
    };
    let ranked = !short && options.sort == SearchSort::Rank;
    let (score, order) = match (ranked, options.sort) {
        (true, _) => (
            format!("bm25({table})"),
            "score, m.created_at DESC".to_owned(),
        ),
        (false, SearchSort::Oldest) => (
            "NULL".to_owned(),
            "m.created_at ASC, m.rowid ASC".to_owned(),
        ),
        (false, _) => (
            "NULL".to_owned(),
            "m.created_at DESC, m.rowid DESC".to_owned(),
        ),
    };
    params.extend(filters.params.iter().cloned());
    params.push(Value::Integer(i64::from(options.limit)));
    params.push(Value::Integer(i64::from(options.offset)));
    let sql_text = format!(
        "SELECT {HIT_COLUMNS}, {score} AS score FROM {table} \
         JOIN messages m ON m.rowid = {table}.rowid JOIN sessions s ON s.id = m.session_id \
         WHERE {condition}{} ORDER BY {order} LIMIT ? OFFSET ?",
        filters.clause
    );
    let mut stmt = conn.prepare(&sql_text).map_err(sql)?;
    let mut rows = stmt.query(params_from_iter(params)).map_err(sql)?;
    let mut hits = Vec::new();
    while let Some(row) = rows.next().map_err(sql)? {
        hits.push(hit_from_row(row, blob_dir, &terms)?);
    }
    Ok(SearchResults { hits, mode })
}

fn escape_like(term: &str) -> String {
    term.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// Canonical-row scan: every term must occur (case-insensitively).
fn scan(
    conn: &Connection,
    blob_dir: &std::path::Path,
    query: &Query,
    filters: &Filters,
    options: &SearchOptions,
) -> Result<SearchResults, Error> {
    let terms = query.terms();
    let order = if options.sort == SearchSort::Oldest {
        "ASC"
    } else {
        "DESC"
    };
    let sql_text = format!(
        "SELECT {HIT_COLUMNS}, NULL AS score FROM messages m JOIN sessions s ON s.id = m.session_id \
         WHERE (m.content IS NOT NULL OR m.content_ref IS NOT NULL){} \
         ORDER BY m.created_at {order}, m.rowid {order}",
        filters.clause
    );
    let mut stmt = conn.prepare(&sql_text).map_err(sql)?;
    let mut rows = stmt
        .query(params_from_iter(filters.params.iter()))
        .map_err(sql)?;
    let wanted = usize::try_from(options.offset + options.limit).unwrap_or(usize::MAX);
    let mut hits = Vec::new();
    while hits.len() < wanted {
        let Some(row) = rows.next().map_err(sql)? else {
            break;
        };
        let text = content(row, blob_dir)?;
        if terms.iter().all(|term| find_ci(&text, term).is_some()) {
            hits.push(hit_from_row(row, blob_dir, &terms)?);
        }
    }
    let skip = usize::try_from(options.offset).unwrap_or(usize::MAX);
    Ok(SearchResults {
        hits: hits.into_iter().skip(skip).collect(),
        mode: SearchMode::Scan,
    })
}

fn content(row: &Row<'_>, blob_dir: &std::path::Path) -> Result<String, Error> {
    if let Some(text) = row.get::<_, Option<String>>(5).map_err(sql)? {
        return Ok(text);
    }
    let Some(reference) = row.get::<_, Option<String>>(6).map_err(sql)? else {
        return Ok(String::new());
    };
    let blob = BlobRef::parse(&reference).ok_or_else(|| corrupt("content_ref", &reference))?;
    Ok(String::from_utf8_lossy(&blobs::get(blob_dir, &blob)?).into_owned())
}

fn hit_from_row(
    row: &Row<'_>,
    blob_dir: &std::path::Path,
    terms: &[&str],
) -> Result<SearchHit, Error> {
    let text = content(row, blob_dir)?;
    Ok(SearchHit {
        session_id: SessionId::parse(&row.get::<_, String>(0).map_err(sql)?)?,
        session_title: row.get(1).map_err(sql)?,
        message_id: MessageId::parse(&row.get::<_, String>(2).map_err(sql)?)?,
        role: row.get::<_, String>(3).map_err(sql)?.parse()?,
        created_at: Timestamp::from_millisecond(row.get(4).map_err(sql)?)
            .unwrap_or(Timestamp::UNIX_EPOCH),
        snippet: snippet(&text, terms),
        score: row.get(7).map_err(sql)?,
    })
}

/// Char index of the first case-insensitive occurrence of `needle`.
fn find_ci(haystack: &str, needle: &str) -> Option<usize> {
    let hay: Vec<char> = haystack.chars().map(fold).collect();
    let needle: Vec<char> = needle.chars().map(fold).collect();
    find_folded(&hay, &needle, 0)
}

fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

fn find_folded(hay: &[char], needle: &[char], from: usize) -> Option<usize> {
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    (from..=hay.len() - needle.len()).find(|&i| hay[i..i + needle.len()] == *needle)
}

/// Up to [`SNIPPET_CHARS`] characters around the first match, matches
/// marked, cut ends shown as `…`.
pub(crate) fn snippet(text: &str, terms: &[&str]) -> String {
    let chars: Vec<char> = text
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .collect();
    let folded: Vec<char> = chars.iter().copied().map(fold).collect();
    let needles: Vec<Vec<char>> = terms
        .iter()
        .map(|t| t.chars().map(fold).collect())
        .collect();
    let first = needles
        .iter()
        .filter_map(|n| find_folded(&folded, n, 0))
        .min()
        .unwrap_or(0);
    let budget = SNIPPET_CHARS - 2;
    let start = first.saturating_sub(budget / 3);
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    let mut i = start;
    while i < chars.len() {
        let matched = needles
            .iter()
            .filter(|n| !n.is_empty() && folded[i..].starts_with(n))
            .map(Vec::len)
            .max();
        let piece_len = matched.map_or(1, |len| len + 2);
        if out.chars().count() + piece_len > budget {
            out.push('…');
            break;
        }
        if let Some(len) = matched {
            out.push(MATCH_START);
            out.extend(&chars[i..i + len]);
            out.push(MATCH_END);
            i += len;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{SNIPPET_CHARS, find_ci, has_cjk, snippet};

    #[test]
    fn snippets_mark_matches_and_stay_short() {
        let text = format!(
            "{} the Kubernetes cluster {}",
            "a ".repeat(300),
            "b ".repeat(300)
        );
        let cut = snippet(&text, &["kubernetes"]);
        assert!(cut.contains("«Kubernetes»"), "{cut}");
        assert!(
            cut.chars().count() <= SNIPPET_CHARS,
            "{}",
            cut.chars().count()
        );
        assert!(cut.starts_with('…') && cut.ends_with('…'), "{cut}");
        assert_eq!(snippet("short text", &["text"]), "short «text»");
    }

    #[test]
    fn matching_ignores_case_and_detects_cjk() {
        assert_eq!(find_ci("Hello World", "world"), Some(6));
        assert!(has_cjk("設定ファイル"));
        assert!(!has_cjk("config file"));
    }
}

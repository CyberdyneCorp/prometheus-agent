//! The semantic side of session search (spec: session-storage, Hybrid
//! semantic and full-text search): with `sessions.search.semantic = true`
//! active user and assistant messages are embedded in the background — the
//! daemon's per-profile loop, `prometheus cron tick`, and a CLI hybrid
//! search before it runs; never inside a turn — with the profile's
//! embedding model (`sessions.search.embedding_model`, else
//! `memory.recall.embedding_model`). Text bound for a remote model is
//! redacted first. A hybrid search embeds only its query; any failure
//! degrades to full text with `degraded = "semantic_unavailable"`.

use std::path::PathBuf;
use std::sync::Arc;

use pmt_agent::tools::session_search::{QueryEmbedder, QueryFuture};
use pmt_config::ProfileScope;
use pmt_core::env::LaunchEnv;
use pmt_core::error::Error;
use pmt_core::secret::Redactor;
use pmt_memory::rank::Embedder;
use pmt_store::search::hybrid::QueryVector;
use pmt_store::{SearchOptions, SearchResults, Store};

use crate::chat::session::model::{Models, RouteFlags};

/// Messages embedded per request.
const BATCH: u32 = 64;
/// Messages one background pass embeds at most.
const PER_PASS: u32 = 1024;

/// The profile's message embedder, when semantic search is on and a model
/// is configured.
pub(crate) fn embedder(
    scope: &ProfileScope,
    store: &Arc<Store>,
    user_home: Option<&std::path::Path>,
) -> Option<Arc<dyn Embedder>> {
    if !scope.config().sessions.search.semantic {
        return None;
    }
    let models = Models::open(scope, store, user_home, &RouteFlags::default(), None)
        .inspect_err(|e| tracing::warn!(error = %e, "semantic session search unavailable"))
        .ok()?;
    models.session_embedder()
}

fn outbound(embedder: &dyn Embedder, redactor: &Redactor, text: &str) -> String {
    if embedder.remote() {
        redactor.redact_all(text)
    } else {
        text.to_owned()
    }
}

/// Embeds up to `limit` messages that have no vector yet; how many.
///
/// # Errors
///
/// The embedding call's or the store's failure (what was stored stays).
pub(crate) async fn fill(
    store: &Store,
    embedder: &dyn Embedder,
    redactor: &Redactor,
    limit: u32,
) -> Result<usize, Error> {
    let model = embedder.model();
    let mut done = 0usize;
    while u32::try_from(done).unwrap_or(u32::MAX) < limit {
        let pending = store.unembedded_messages(&model, BATCH)?;
        if pending.is_empty() {
            break;
        }
        let inputs = pending
            .iter()
            .map(|m| outbound(embedder, redactor, &m.text))
            .collect();
        let vectors = embedder.embed(inputs).await?;
        let rows: Vec<_> = pending
            .into_iter()
            .zip(vectors)
            .map(|(m, v)| (m.message_id, m.text, v))
            .collect();
        done += rows.len();
        store.put_message_embeddings(&model, &rows)?;
    }
    Ok(done)
}

/// A hybrid search: the query embedded with `embedder` (none: degraded).
///
/// # Errors
///
/// As `Store::hybrid_search`.
pub(crate) async fn hybrid(
    store: &Store,
    embedder: Option<&dyn Embedder>,
    redactor: &Redactor,
    query: &str,
    options: &SearchOptions,
) -> Result<SearchResults, Error> {
    let vector = match embedder {
        Some(embedder) => match embedder
            .embed(vec![outbound(embedder, redactor, query)])
            .await
        {
            Ok(mut vectors) => vectors.pop().map(|v| (embedder.model(), v)),
            Err(e) => {
                tracing::warn!(error = %e, "query not embedded; full-text search only");
                None
            }
        },
        None => None,
    };
    let query_vector = vector
        .as_ref()
        .map(|(model, vector)| QueryVector { model, vector });
    store.hybrid_search(query, options, query_vector)
}

/// The daemon's (and `cron tick`'s) background pass for one profile, on
/// its own thread when semantic search is on.
pub(crate) fn check(
    scope: &ProfileScope,
    store: &Arc<Store>,
    launch: &LaunchEnv,
) -> Option<std::thread::JoinHandle<()>> {
    if !scope.config().sessions.search.semantic {
        return None;
    }
    let (scope, store, launch) = (scope.clone(), Arc::clone(store), launch.clone());
    std::thread::Builder::new()
        .name("pmt-session-embeddings".to_owned())
        .spawn(move || {
            let Some(embedder) = embedder(&scope, &store, launch.user_home().map(PathBuf::as_path))
            else {
                return;
            };
            let redactor = scope.secrets().redactor().unwrap_or_default();
            let filled =
                crate::model_cmd::block_on(fill(&store, embedder.as_ref(), &redactor, PER_PASS));
            match filled {
                Ok(Ok(n)) if n > 0 => tracing::info!(messages = n, "session messages embedded"),
                Ok(Ok(_)) => {}
                Ok(Err(e)) | Err(e) => tracing::warn!(error = %e, "session embedding pass failed"),
            }
        })
        .ok()
}

/// The `session_search` tool's query embedder.
pub(crate) struct ToolQueries {
    pub(crate) embedder: Arc<dyn Embedder>,
    pub(crate) redactor: Redactor,
}

impl QueryEmbedder for ToolQueries {
    fn model(&self) -> String {
        self.embedder.model()
    }

    fn embed_query<'a>(&'a self, text: &'a str) -> QueryFuture<'a> {
        Box::pin(async move {
            let input = outbound(self.embedder.as_ref(), &self.redactor, text);
            match self.embedder.embed(vec![input]).await {
                Ok(mut vectors) => vectors.pop(),
                Err(e) => {
                    tracing::warn!(error = %e, "session_search query not embedded");
                    None
                }
            }
        })
    }
}

/// `search` (a session's `session_search`) answering hybrid searches when
/// semantic search is on and an embedding model is configured.
pub(crate) fn tool(
    search: pmt_agent::tools::session_search::SessionSearch,
    (scope, store): (&ProfileScope, &Arc<Store>),
    user_home: Option<&std::path::Path>,
) -> pmt_agent::tools::session_search::SessionSearch {
    match embedder(scope, store, user_home) {
        Some(embedder) => search.with_queries(Arc::new(ToolQueries {
            embedder,
            redactor: scope.secrets().redactor().unwrap_or_default(),
        })),
        None => search,
    }
}

/// Embeds what is pending before a CLI hybrid search (outside any turn).
pub(crate) fn fill_now(scope: &ProfileScope, store: &Store, embedder: &dyn Embedder) {
    let redactor = scope.secrets().redactor().unwrap_or_default();
    match crate::model_cmd::block_on(fill(store, embedder, &redactor, PER_PASS)) {
        Ok(Ok(_)) => {}
        Ok(Err(e)) | Err(e) => tracing::warn!(error = %e, "session embedding pass failed"),
    }
}

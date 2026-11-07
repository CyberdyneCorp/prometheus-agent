//! The embedder of semantic memory recall (spec: memory, Semantic recall):
//! `memory.recall.embedding_model` resolved like any model reference (an
//! alias, `provider/model`, or a bare model on the session's provider) on
//! first use, then `POST <base_url>/embeddings` on that route.

use std::sync::Arc;

use pmt_agent::turn::model::BoxFuture;
use pmt_core::error::Error;
use pmt_memory::rank::Embedder;
use pmt_providers::resolve::{Route, Trace};
use tokio::sync::OnceCell;

use super::model::ProviderSetupParts;

/// Embeddings on the route of one model reference.
pub(super) struct RouteEmbedder {
    parts: Arc<ProviderSetupParts>,
    client: reqwest::Client,
    spec: String,
    provider: Option<String>,
    route: OnceCell<Route>,
}

impl RouteEmbedder {
    pub(super) fn new(
        parts: Arc<ProviderSetupParts>,
        client: reqwest::Client,
        spec: String,
        provider: Option<String>,
    ) -> Self {
        Self {
            parts,
            client,
            spec,
            provider,
            route: OnceCell::new(),
        }
    }

    async fn route(&self) -> Result<&Route, Error> {
        self.route
            .get_or_try_init(|| async {
                let resolver = self.parts.resolver();
                let target = pmt_providers::target::parse(
                    &self.spec,
                    &self.parts.config.model,
                    resolver.registry,
                    self.provider.as_deref(),
                )?;
                resolver
                    .resolve_target(&target, &mut Trace::default())
                    .await
            })
            .await
    }
}

impl Embedder for RouteEmbedder {
    fn model(&self) -> String {
        // The configured reference keys stored vectors: stable before the
        // route is resolved, and a changed setting recomputes them.
        self.spec.clone()
    }

    fn remote(&self) -> bool {
        // Until resolved, assume remote: redacting is the safe side.
        self.route.get().is_none_or(|r| !r.local)
    }

    fn embed(&self, inputs: Vec<String>) -> BoxFuture<'_, Result<Vec<Vec<f32>>, Error>> {
        Box::pin(async move {
            let route = self.route().await?;
            pmt_providers::embeddings::embed(&self.client, route, &inputs).await
        })
    }
}

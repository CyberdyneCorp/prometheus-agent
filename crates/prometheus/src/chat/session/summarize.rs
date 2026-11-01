//! The session's side-model summaries (spec: context-compression,
//! Summary generation; session-storage, Session search tool): compaction
//! summaries and `session_search`'s per-session summaries both run on the
//! `compression` auxiliary route. The route is resolved at each call
//! against the session's current main route, so a `/model` switch is
//! followed and a session that never compacts never resolves it.

use std::sync::Arc;

use pmt_agent::compress::summary::{AuxSummarizer, classify};
use pmt_agent::compress::{FailureKind, Summarizer, SummaryFailure};
use pmt_agent::tools::session_search;
use pmt_agent::turn::model::BoxFuture;
use pmt_core::id::SessionId;
use pmt_providers::auxiliary::{self, AuxArgs, AuxCaller, AuxContext, AuxRoute, AuxTask};
use pmt_providers::cost::CallContext;
use pmt_providers::fallback::Healthy;
use pmt_providers::request::{Message, Request, Role};
use pmt_providers::resolve::Trace;

use super::model::{ProviderSetupParts, SwitchableModel};

/// Output cap of one `session_search` summary.
const SEARCH_SUMMARY_TOKENS: u64 = 300;

/// Summaries on the session's `compression` auxiliary route.
pub struct RouteSummarizer {
    pub(super) parts: Arc<ProviderSetupParts>,
    pub(super) aux: Arc<AuxCaller>,
    pub(super) model: Arc<SwitchableModel>,
    pub(super) session: SessionId,
}

impl RouteSummarizer {
    /// The `compression` route against the current main route; `None`
    /// when no route answers.
    async fn route(&self) -> Result<AuxRoute, SummaryFailure> {
        let resolver = self.parts.resolver();
        let main = resolver
            .resolve_target(&self.model.target(), &mut Trace::default())
            .await
            .ok();
        let ctx = AuxContext {
            resolver: &resolver,
            main: main.as_ref(),
            health: &Healthy,
        };
        match auxiliary::resolve(AuxTask::Compression, &AuxArgs::default(), &ctx).await {
            Ok(Some(route)) => Ok(route),
            Ok(None) => Err(SummaryFailure::new(
                FailureKind::Failed,
                "no compression route answers",
            )),
            Err(err) => Err(classify(&err)),
        }
    }

    async fn call(&self, request: &Request) -> Result<String, SummaryFailure> {
        let route = self.route().await?;
        let context = CallContext::new(self.session, &AuxTask::Compression.purpose());
        AuxSummarizer::new(Arc::clone(&self.aux), route, context, None)
            .summarize(request)
            .await
    }
}

impl Summarizer for RouteSummarizer {
    fn summarize<'a>(
        &'a self,
        request: &'a Request,
    ) -> BoxFuture<'a, Result<String, SummaryFailure>> {
        Box::pin(self.call(request))
    }

    /// The window of a model pinned by `[auxiliary.compression]`; an
    /// unpinned route follows the main model, whose window already bounds
    /// the trigger.
    fn context_window(&self) -> Option<u64> {
        let pinned = self.parts.config.auxiliary.compression.as_ref()?;
        let (provider, model) = (pinned.provider.as_deref()?, pinned.model.as_deref()?);
        Some(
            self.parts
                .catalog
                .metadata(provider, model)
                .0
                .context_window,
        )
    }
}

impl session_search::Summarizer for RouteSummarizer {
    fn summarize<'a>(
        &'a self,
        query: &'a str,
        _session: SessionId,
        snippets: &'a [String],
    ) -> session_search::SummaryFuture<'a> {
        Box::pin(async move {
            let request = search_request(query, snippets);
            self.call(&request)
                .await
                .inspect_err(|err| tracing::debug!(error = %err.message, "session summary failed"))
                .ok()
        })
    }
}

/// The request summarising one session's hits for `query`.
fn search_request(query: &str, snippets: &[String]) -> Request {
    let text = format!(
        "In two sentences, say what this past session says about \"{query}\". \
         Use only these excerpts:\n\n{}",
        snippets.join("\n---\n")
    );
    Request {
        messages: vec![Message::text(Role::User, text)],
        max_output_tokens: Some(SEARCH_SUMMARY_TOKENS),
        ..Request::default()
    }
}

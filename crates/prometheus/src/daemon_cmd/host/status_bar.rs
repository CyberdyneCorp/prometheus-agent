//! The context meter of the desktop status bar (spec: desktop-app,
//! Desktop status bar): `session.context_usage` broken down by category.

use pmt_agent::turn::model::{ModelPort as _, RouteInfo};
use pmt_rpc::contracts::ContextCategory;

use super::{DaemonHost, Loaded};

impl DaemonHost {
    /// The loaded session's route and its context by category (none when
    /// the prompt cannot be measured; the meter then shows the plain
    /// estimate).
    pub(super) async fn breakdown(
        session: &Loaded,
        conversation_bytes: usize,
    ) -> (RouteInfo, Vec<ContextCategory>) {
        let route = session.live.model.route().await;
        let scope = session.live.config.get();
        let categories = match session
            .live
            .agent
            .context_breakdown(scope.config(), &route, conversation_bytes)
            .await
        {
            Ok(categories) => categories
                .into_iter()
                .map(|(category, tokens)| ContextCategory {
                    category: category.to_owned(),
                    tokens,
                })
                .collect(),
            Err(err) => {
                tracing::warn!(error = %err, "context breakdown unavailable");
                Vec::new()
            }
        };
        (route, categories)
    }
}

//! A context engine's per-request selection (spec: memory, Context engine
//! selection): before each model request a plugin engine may choose which
//! stored rows the request sends. The choice shapes only that outgoing
//! request; the stored transcript keeps every message. A failing engine
//! sends everything (logged).

use pmt_core::error::Error;
use pmt_store::Visibility;

use super::model::RouteInfo;
use super::run::Turn;

impl Turn<'_> {
    /// Asks the session's context engine, if any, for the next request's
    /// rows.
    pub(super) async fn select_context(&mut self, route: &RouteInfo) -> Result<(), Error> {
        self.selected = None;
        let Some(engine) = self.agent.setup.context_engine.clone() else {
            return Ok(());
        };
        let setup = &self.agent.setup;
        let rows = setup.store.messages(setup.session, Visibility::Active)?;
        let budget = self.agent.trigger_tokens(self.config, route);
        match engine.select(&rows, budget).await {
            Ok(choice) => self.selected = choice.map(|ids| ids.into_iter().collect()),
            Err(err) => {
                tracing::warn!(
                    "context engine selection failed; sending the whole transcript: {err}"
                );
            }
        }
        Ok(())
    }
}

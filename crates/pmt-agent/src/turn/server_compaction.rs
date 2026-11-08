//! Server-side Responses compaction (spec: context-compression,
//! Server-side Responses compaction (opt-in)).
//!
//! With `compression.responses_native = true` on a route whose provider
//! compacts server-side, every request carries the provider's compaction
//! setting. A response may return an opaque, encrypted compaction item
//! standing for the request's whole input: it is stored as a blob linked
//! to the session and named in the metadata of the reply's row together
//! with the last row it covers. Later requests send that item first in
//! place of the covered rows, while the stored transcript keeps every
//! message. A local compaction that folds the covered row ends the replay
//! (the item no longer matches the transcript); local compaction keeps
//! running at its own trigger. A route without support ignores the
//! setting with one INFO log per session.

use std::sync::atomic::Ordering;

use pmt_config::Config;
use pmt_core::error::Error;
use pmt_providers::request::ServerCompaction;
use pmt_store::{BlobRef, Message as Row, NewMessage};
use serde_json::{Value, json};

use super::Agent;
use super::model::{ModelReply, RouteInfo};
use super::run::Turn;

/// The reply row's metadata key naming the item and the rows it covers.
const METADATA_KEY: &str = "server_compaction";
/// The blob reference owner: the item is linked to the session.
const OWNER: &str = "session";
/// Headroom below the local trigger for the default server threshold.
const BELOW_LOCAL: u64 = 8192;
/// The server threshold when no local trigger is known.
const NO_LOCAL_TRIGGER: u64 = 200_000;

/// The server threshold: the configured one, else the local trigger minus
/// 8192 tokens, else 200000.
#[must_use]
fn threshold(config: &Config, local_trigger: Option<u64>) -> u64 {
    match config.compression.responses_compact_threshold_tokens {
        0 => local_trigger.map_or(NO_LOCAL_TRIGGER, |t| t.saturating_sub(BELOW_LOCAL).max(1)),
        set => set,
    }
}

/// The newest stored item still matching the transcript: the item and
/// the index of the first row after the history it covers.
fn latest_item(rows: &[Row]) -> Option<(BlobRef, usize)> {
    rows.iter().rev().find_map(|row| {
        let meta = row.metadata.as_ref()?.get(METADATA_KEY)?;
        let blob = BlobRef::parse(meta.get("blob")?.as_str()?)?;
        let covers = meta.get("covers")?.as_str()?;
        let at = rows.iter().position(|r| r.id.to_string() == covers)?;
        Some((blob, at + 1))
    })
}

impl Agent {
    /// Whether this session asks `route` for server-side compaction; a
    /// route without support is logged once per session.
    fn server_compaction_on(&self, config: &Config, route: &RouteInfo) -> bool {
        if !config.compression.responses_native {
            return false;
        }
        if !route.server_compaction && !self.server_compaction_logged.swap(true, Ordering::Relaxed)
        {
            tracing::info!(
                provider = %route.provider,
                model = %route.model,
                "compression.responses_native is ignored: the route has no server-side compaction"
            );
        }
        route.server_compaction
    }
}

impl Turn<'_> {
    /// The server-side compaction of the next request over `rows` and the
    /// index of the first row the request still sends; `(None, 0)` when it
    /// is off for this route. An unreadable item sends the full history.
    pub(super) fn server_compaction(
        &mut self,
        route: &RouteInfo,
        rows: &[Row],
    ) -> (Option<ServerCompaction>, usize) {
        self.covers = None;
        if !self.agent.server_compaction_on(self.config, route) {
            return (None, 0);
        }
        self.covers = rows.last().map(|r| r.id);
        let local = self
            .config
            .compression
            .enabled
            .then(|| self.agent.trigger_tokens(self.config, route));
        let mut compaction = ServerCompaction {
            threshold: threshold(self.config, local),
            item: None,
        };
        let Some((blob, from)) = latest_item(rows) else {
            return (Some(compaction), 0);
        };
        let item = self
            .agent
            .setup
            .store
            .get_blob(&blob)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
        if item.is_none() {
            tracing::warn!(blob = %blob, "server compaction item unreadable; sending the full history");
            return (Some(compaction), 0);
        }
        compaction.item = item;
        (Some(compaction), from)
    }

    /// Keeps the compaction item `reply` returned: a blob linked to the
    /// session, named in `row`'s metadata with the last row the request
    /// covered.
    pub(super) fn keep_compaction_item(
        &self,
        reply: &ModelReply,
        row: &mut NewMessage,
    ) -> Result<(), Error> {
        let (Some(item), Some(covers)) = (&reply.compaction_item, self.covers) else {
            return Ok(());
        };
        let setup = &self.agent.setup;
        let bytes = serde_json::to_vec(item).unwrap_or_default();
        let blob = setup.store.put_blob(&bytes)?;
        setup
            .store
            .reference_blob(&blob, OWNER, &setup.session.to_string())?;
        let mut metadata = row.metadata.take().unwrap_or_else(|| json!({}));
        metadata[METADATA_KEY] = json!({"blob": blob.to_string(), "covers": covers.to_string()});
        row.metadata = Some(metadata);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_threshold_sits_below_the_local_trigger() {
        let mut config = Config::default();
        assert_eq!(threshold(&config, Some(100_000)), 91_808);
        assert_eq!(threshold(&config, None), 200_000);
        config.compression.responses_compact_threshold_tokens = 5000;
        assert_eq!(threshold(&config, Some(100_000)), 5000);
    }
}

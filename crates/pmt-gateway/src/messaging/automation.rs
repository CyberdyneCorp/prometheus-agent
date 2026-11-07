//! What the daemon's scheduled jobs need of the gateway beyond sending
//! (spec: scheduled-jobs, Continuable deliveries; Bot Chat delivery
//! target): where an adapter lets a continuation land, and a link for a
//! turn the daemon itself starts (no client or chat asked for it).

use crate::Link;

use super::Gateway;
use super::descriptor::ContinuableSurface;

impl Gateway {
    /// Where `platform`'s adapter of `profile` lets a continuable delivery
    /// land: `in_channel` only where it declares that surface.
    #[must_use]
    pub fn continuable_surface(&self, profile: &str, platform: &str) -> ContinuableSurface {
        self.shared
            .adapters
            .descriptor(profile, platform)
            .map(|d| d.continuable_surface)
            .unwrap_or_default()
    }

    /// A link to `session_id`'s attached clients, for a turn the daemon
    /// starts on its own (a scheduled delivery admitted into a Bot Chat):
    /// whoever watches the session sees it run.
    #[must_use]
    pub fn link(&self, session_id: &str) -> Link {
        self.shared.server.link(session_id)
    }
}

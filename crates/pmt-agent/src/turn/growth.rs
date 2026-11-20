//! Growing a local model's window before compressing (spec: local-models,
//! Context window and memory placement; context-compression): when the
//! conversation reaches the compression threshold on a local route, the
//! session's [`WindowGrowth`] port may relaunch the model at a larger
//! window; the turn then sends the request on the grown window and does
//! not compress. When it says growth is impossible (no fit, too slow, the
//! native maximum), compression runs as usual.

use std::sync::Arc;

use super::model::{BoxFuture, RouteInfo};
use super::run::Turn;

/// What a growth attempt achieved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Growth {
    /// The model now runs with `to` tokens (it ran with `from`).
    Grown {
        /// The window before.
        from: u64,
        /// The window now.
        to: u64,
    },
    /// The window cannot grow; why (`native_max`, `no_fit`,
    /// `below_speed_floor`, or not a growable route).
    Impossible(String),
}

/// Relaunches a session's local model at a larger window.
pub trait WindowGrowth: Send + Sync {
    /// Grows `route`'s window of `session` to hold `needed` tokens.
    fn grow<'a>(
        &'a self,
        session: &'a str,
        route: &'a RouteInfo,
        needed: u64,
    ) -> BoxFuture<'a, Growth>;
}

/// The notice a person sees when the window grew.
pub const GROWN_NOTICE: &str = "local model window grown";

impl Turn<'_> {
    /// Asks the growth port to make room for `tokens` on a local route;
    /// `true` when the window grew (compression is skipped).
    pub(super) async fn grow_window(&mut self, route: &RouteInfo, tokens: u64) -> bool {
        if !route.local {
            return false;
        }
        let Some(growth) = self.agent.setup.growth.as_ref().map(Arc::clone) else {
            return false;
        };
        let session = self.agent.setup.session.to_string();
        match growth.grow(&session, route, tokens).await {
            Growth::Grown { from, to } => {
                tracing::info!(from, to, "local model window grown instead of compressing");
                self.notices
                    .push(format!("{GROWN_NOTICE}: {from} -> {to} tokens"));
                true
            }
            Growth::Impossible(why) => {
                tracing::debug!(why, "local model window cannot grow; compressing");
                false
            }
        }
    }
}

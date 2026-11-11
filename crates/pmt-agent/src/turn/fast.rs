//! Which requests of a turn ask for the priority tier (spec:
//! model-providers, Fast mode service tier): `agent.fast_mode`, or the
//! session's `/fast` choice, decides per request from the time since the
//! turn started; a route without a tier is told about once per session.

use std::sync::atomic::Ordering;
use std::time::Duration;

use pmt_config::Config;
use pmt_config::schema::FastMode;
use pmt_providers::fast::Speed;

use super::Agent;
use super::model::RouteInfo;
use super::run::Turn;

/// The catalog key of the notice of a fast mode a route cannot honour.
pub const NO_TIER_NOTICE: &str = "display.fast_no_tier";

impl Agent {
    /// The session's fast mode: its `/fast` choice, else `agent.fast_mode`.
    #[must_use]
    pub fn fast_mode(&self, config: &Config) -> FastMode {
        let chosen = *self
            .fast
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        chosen.unwrap_or(config.agent.fast_mode)
    }

    /// The session's own `/fast` choice, if any.
    #[must_use]
    pub fn fast_override(&self) -> Option<FastMode> {
        *self
            .fast
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// `/fast <mode>` for this session (`None`: back to the config).
    pub fn set_fast_mode(&self, mode: Option<FastMode>) {
        *self
            .fast
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = mode;
        self.fast_noticed.store(false, Ordering::Relaxed);
    }
}

impl Turn<'_> {
    /// The speed the next request asks for on `route`.
    pub(super) fn speed(&mut self, route: &RouteInfo) -> Option<Speed> {
        let mode = self.agent.fast_mode(self.config);
        let window = Duration::from_secs(self.config.agent.fast_auto_secs);
        let speed = pmt_providers::fast::wanted(
            mode,
            self.started.elapsed(),
            self.prior_rows == 0,
            window,
        )?;
        if route.fast_tier {
            return Some(speed);
        }
        if !self.agent.fast_noticed.swap(true, Ordering::Relaxed) {
            tracing::info!(provider = %route.provider, model = %route.model, "fast mode ignored: no priority tier");
            self.notices
                .push(self.strings.get(NO_TIER_NOTICE).to_owned());
        }
        None
    }
}

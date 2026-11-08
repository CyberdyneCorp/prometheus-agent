//! Idle compaction (spec: context-compression, Idle compaction).
//!
//! A conversation picked up again after a long pause has usually lost its
//! prompt cache, so its whole history would be paid for again at full
//! price. With `compression.idle_compact_after_secs` set, the first model
//! call of a turn that starts at least that long after the session's last
//! row is preceded by a compaction with trigger `idle`, unless the context
//! is already small (at or below `threshold * target_ratio`: the size a
//! compaction would aim for anyway). The attempt goes through the same
//! lock, cooldown, backoff and per-turn attempt budget as every automatic
//! compaction. It also measures the context pressure each turn reports.

use jiff::{SignedDuration, Timestamp};
use pmt_core::error::Error;
use pmt_store::Visibility;

use super::model::RouteInfo;
use super::run::Turn;
use crate::compress::Trigger;
use crate::compress::decision::{self, Decision};
use crate::prompt::frozen::Frozen;

/// The context size at or below which an idle session is left alone.
#[must_use]
pub(super) fn floor(trigger_tokens: u64, target_ratio: f64) -> u64 {
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let floor = (trigger_tokens as f64 * target_ratio) as u64;
    floor
}

/// Whether a turn starting `now` after `last` activity is past the idle
/// period of `after_secs` (`0` is off).
#[must_use]
pub(super) fn idle(last: Option<Timestamp>, now: Timestamp, after_secs: u64) -> bool {
    let Some(last) = last else {
        return false;
    };
    after_secs > 0
        && now.duration_since(last)
            >= SignedDuration::from_secs(i64::try_from(after_secs).unwrap_or(i64::MAX))
}

impl Turn<'_> {
    /// The context size after the turn and the compaction trigger, for
    /// the context pressure gauge (`turn.end`); `(None, None)` when the
    /// store cannot say.
    pub(super) async fn pressure(&self) -> (Option<u64>, Option<u64>) {
        let setup = &self.agent.setup;
        let Ok(rows) = setup.store.messages(setup.session, Visibility::Active) else {
            return (None, None);
        };
        let Ok(known) = self.agent.known_context(&rows) else {
            return (None, None);
        };
        let route = setup.model.route().await;
        (
            Some(known.tokens()),
            Some(self.agent.trigger_tokens(self.config, &route)),
        )
    }

    /// When the session last saw activity: its newest active row, read
    /// before this turn adds its own.
    pub(super) fn last_activity(&self) -> Result<Option<Timestamp>, Error> {
        let setup = &self.agent.setup;
        Ok(setup
            .store
            .messages(setup.session, Visibility::Active)?
            .iter()
            .map(|r| r.created_at)
            .max())
    }

    /// The idle compaction before the turn's first model call; a commit
    /// refreezes `frozen`.
    pub(super) async fn idle_compaction(
        &mut self,
        frozen: &mut Frozen,
        route: &RouteInfo,
        last: Option<Timestamp>,
    ) -> Result<(), Error> {
        let compression = &self.config.compression;
        let now = Timestamp::now();
        if !compression.enabled
            || self.compression_attempts >= compression.max_attempts
            || !idle(last, now, compression.idle_compact_after_secs)
        {
            return Ok(());
        }
        let setup = &self.agent.setup;
        let rows = setup.store.messages(setup.session, Visibility::Active)?;
        let tokens = self.agent.known_context(&rows)?.tokens();
        let floor = floor(
            self.agent.trigger_tokens(self.config, route),
            compression.target_ratio,
        );
        if tokens <= floor {
            tracing::debug!(
                tokens,
                floor,
                "idle compaction skipped: the context is small"
            );
            return Ok(());
        }
        let state = setup.store.compression_state(setup.session)?;
        match decision::should_compress(tokens, floor, &state, now) {
            Decision::Run => {}
            other => {
                tracing::debug!(decision = %other, tokens, "idle compaction not run");
                return Ok(());
            }
        }
        self.attempt(Trigger::Idle, frozen, route, Some(tokens))
            .await
            .map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_needs_the_whole_period() {
        let now: Timestamp = "2026-11-07T12:00:00Z".parse().unwrap();
        let three_hours_ago = now - SignedDuration::from_hours(3);
        assert!(idle(Some(three_hours_ago), now, 1800));
        assert!(!idle(Some(three_hours_ago), now, 0), "0 is off");
        assert!(!idle(Some(now - SignedDuration::from_mins(10)), now, 1800));
        assert!(!idle(None, now, 1800), "a new session has no pause");
        assert_eq!(floor(10_000, 0.2), 2000);
    }
}

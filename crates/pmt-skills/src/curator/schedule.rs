//! When an automatic curator pass runs (spec: skills, Curator
//! configuration and scheduling): enabled, not paused, the last pass
//! older than `interval_secs`, and the profile idle for `min_idle_secs`.
//! The first observation only seeds the last-run time, so a new profile
//! is not curated the moment it appears.

use jiff::{SignedDuration, Timestamp};
use pmt_config::schema::CuratorConfig;
use pmt_core::error::Error;
use pmt_store::Store;

/// Whether a pass is due.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Due {
    /// Run now.
    Now,
    /// The first observation: the last-run time was seeded.
    Seeded,
    /// Not now, with why.
    Later(String),
}

fn secs(value: u64) -> SignedDuration {
    SignedDuration::from_secs(i64::try_from(value).unwrap_or(i64::MAX))
}

/// Decides, seeding the last-run time on the first observation;
/// `last_activity` is the profile's latest session activity.
///
/// # Errors
///
/// Store errors.
pub fn due(
    store: &Store,
    config: &CuratorConfig,
    now: Timestamp,
    last_activity: Option<Timestamp>,
) -> Result<Due, Error> {
    if !config.enabled {
        return Ok(Due::Later("curator.enabled is false".to_owned()));
    }
    let state = store.curator_state()?;
    if state.paused {
        return Ok(Due::Later("paused".to_owned()));
    }
    let Some(last_run) = state.last_run_at else {
        store.set_curator_last_run(now)?;
        return Ok(Due::Seeded);
    };
    let next = last_run
        .checked_add(secs(config.interval_secs))
        .unwrap_or(Timestamp::MAX);
    if now < next {
        return Ok(Due::Later(format!("next pass after {next}")));
    }
    if let Some(active) = last_activity {
        let idle_until = active
            .checked_add(secs(config.min_idle_secs))
            .unwrap_or(Timestamp::MAX);
        if now < idle_until {
            return Ok(Due::Later(format!(
                "the profile is active (idle from {idle_until})"
            )));
        }
    }
    Ok(Due::Now)
}

//! The Test and Restart adapter actions of the setup forms (spec:
//! web-dashboard, Channel setup forms; desktop-app, Messaging pane shows
//! saved versus running state). Test builds the adapter from the
//! profile's saved configuration without connecting it and checks that
//! every endpoint it declares answers (the `status --deep` probe); Restart
//! stops that one adapter and starts it again from the saved
//! configuration, so a form save takes effect, leaving every other
//! adapter alone.

use std::sync::Arc;
use std::time::Duration;

use pmt_core::error::{Error, codes};

use super::adapter::{AdapterSetup, AdapterState, AdapterStatus};
use super::probe::{self, Probed};
use super::{Gateway, Shared, adapters};

/// How long a Test may wait for each endpoint.
pub const TEST_WITHIN: Duration = Duration::from_secs(10);
/// How long a Restart waits to report the new state.
const RESTART_REPORT_WAIT: Duration = Duration::from_secs(5);

/// What a Test found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tested {
    /// The adapter only receives: nothing to reach.
    InboundOnly,
    /// Every endpoint answered; the slowest took this long.
    Reached(Duration),
    /// The adapter cannot be built or an endpoint did not answer: why.
    Failed(String),
}

fn unserved(profile: &str) -> Error {
    Error::new(
        &codes::PROFILE_NOT_FOUND,
        format!("profile '{profile}' is not served"),
    )
}

async fn test(shared: &Shared, profile: &str, platform: &str) -> Result<Tested, Error> {
    let scope = shared
        .agents
        .scope(profile)
        .ok_or_else(|| unserved(profile))?;
    let Some(factory) = adapters::factory_of(shared, platform) else {
        return Ok(Tested::Failed(format!(
            "this build has no adapter for platform '{platform}'"
        )));
    };
    let setup = AdapterSetup {
        config: scope.config().gateway.platform(platform),
        scope,
        platform: platform.to_owned(),
    };
    let reach = match factory(&setup) {
        Ok(adapter) => adapter.reach(),
        Err(err) => Err(err.message().to_owned()),
    };
    let outcome = match reach {
        Ok(reach) => probe::probe(&setup, &reach, TEST_WITHIN).await,
        Err(why) => Err(why),
    };
    Ok(match outcome {
        Ok(Probed::InboundOnly) => Tested::InboundOnly,
        Ok(Probed::Reached(took)) => Tested::Reached(took),
        Err(why) => Tested::Failed(why),
    })
}

async fn restart(
    shared: &Arc<Shared>,
    profile: &str,
    platform: &str,
) -> Result<AdapterStatus, Error> {
    let scope = shared
        .agents
        .scope(profile)
        .ok_or_else(|| unserved(profile))?;
    adapters::forget(shared, profile, platform).await;
    let table = scope.config().gateway.platform(platform);
    if table.enabled {
        adapters::start_platform(shared, &scope, platform, &table);
    }
    let deadline = tokio::time::Instant::now() + RESTART_REPORT_WAIT;
    loop {
        let status = shared
            .adapters
            .statuses()
            .into_iter()
            .find(|s| s.profile == profile && s.platform == platform);
        let settled = status
            .as_ref()
            .is_none_or(|s| !matches!(s.state, AdapterState::Connecting | AdapterState::Retrying));
        if settled || tokio::time::Instant::now() >= deadline {
            return Ok(status.unwrap_or_else(|| AdapterStatus {
                profile: profile.to_owned(),
                platform: platform.to_owned(),
                state: AdapterState::Disabled,
                code: None,
                message: None,
                attention: false,
                paused: false,
                descriptor: None,
            }));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

impl Gateway {
    /// Tests `platform` of `profile` as its saved configuration describes
    /// it, without connecting it.
    ///
    /// # Errors
    ///
    /// `profile.not_found` for a profile that is not served.
    pub async fn test_platform(&self, profile: &str, platform: &str) -> Result<Tested, Error> {
        test(&self.shared, profile, platform).await
    }

    /// Restarts `platform` of `profile` from its saved configuration (a
    /// disabled adapter just stops); its state once it settled.
    ///
    /// # Errors
    ///
    /// `profile.not_found` for a profile that is not served.
    pub async fn restart_platform(
        &self,
        profile: &str,
        platform: &str,
    ) -> Result<AdapterStatus, Error> {
        restart(&self.shared, profile, platform).await
    }
}

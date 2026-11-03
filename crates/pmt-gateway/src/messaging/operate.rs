//! Adapter operations (spec: messaging-gateway, Adapter operations from
//! chat): `/platforms` lists each adapter of a profile with its state,
//! error code and pause flag; an admin's `/platform pause <name>` (or
//! `prometheus gateway pause <name>`) stops dispatching that adapter's
//! inbound messages without disconnecting it (they are acknowledged and
//! dropped with admission `paused`; outbound delivery continues), and
//! `/platform resume <name>` clears the pause and the reconnect backoff
//! and reconnects at once. A pause lives in the profile's `state.db`, so
//! it survives restarts.

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use pmt_core::error::{Error, codes};

use super::adapter::{AdapterState, AdapterStatus};
use super::{Gateway, Shared, adapters};

/// How long `/platform resume` waits to report the reconnected state.
const RESUME_REPORT_WAIT: Duration = Duration::from_secs(5);

/// One adapter as `/platforms` shows it.
#[must_use]
pub fn platform_line(status: &AdapterStatus) -> String {
    let state = serde_json::to_value(status.state)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default();
    let mut line = format!("{}: {state}", status.platform);
    if let Some(code) = &status.code {
        let _ = write!(line, " ({code})");
    }
    if status.paused {
        line.push_str(" [paused]");
    }
    if status.attention {
        line.push_str(" [needs attention]");
    }
    line
}

/// Whether an admin paused `platform` of `profile`.
pub(crate) fn is_paused(shared: &Shared, profile: &str, platform: &str) -> bool {
    shared
        .store(profile)
        .is_some_and(|store| matches!(store.adapter_pause(platform), Ok(Some(_))))
}

/// The statuses of `profile`'s adapters, pause flags included.
pub(crate) fn statuses_of(shared: &Shared, profile: &str) -> Vec<AdapterStatus> {
    shared
        .adapters
        .statuses()
        .into_iter()
        .filter(|s| s.profile == profile)
        .map(|mut s| {
            s.paused = is_paused(shared, profile, &s.platform);
            s
        })
        .collect()
}

/// `/platforms`: one line per adapter of `profile`.
pub(crate) fn listing(shared: &Shared, profile: &str) -> String {
    let lines: Vec<String> = statuses_of(shared, profile)
        .iter()
        .map(platform_line)
        .collect();
    if lines.is_empty() {
        "No messaging adapters are enabled for this profile.".to_owned()
    } else {
        lines.join("\n")
    }
}

fn known(shared: &Shared, profile: &str, platform: &str) -> Result<(), Error> {
    let enabled = shared
        .agents
        .scope(profile)
        .is_some_and(|s| s.config().gateway.platform(platform).enabled);
    let running = shared
        .adapters
        .statuses()
        .iter()
        .any(|s| s.profile == profile && s.platform == platform);
    if enabled || running {
        Ok(())
    } else {
        Err(Error::new(
            &codes::GATEWAY_NO_ADAPTER,
            format!("profile '{profile}' has no '{platform}' adapter"),
        ))
    }
}

/// Pauses `platform` of `profile`; `false` when it already was.
pub(crate) fn pause(
    shared: &Shared,
    profile: &str,
    platform: &str,
    actor: &str,
) -> Result<bool, Error> {
    known(shared, profile, platform)?;
    let store = shared.store(profile).ok_or_else(|| unserved(profile))?;
    let paused = store.pause_adapter(platform, actor)?;
    if paused {
        tracing::info!(profile, platform, actor, "adapter paused");
    }
    Ok(paused)
}

/// Clears the pause and the reconnect backoff of `platform` of `profile`
/// and reconnects it now (a fatal adapter is built again); answers its
/// state once it settled, at most a few seconds later.
pub(crate) async fn resume(
    shared: &Arc<Shared>,
    profile: &str,
    platform: &str,
) -> Result<AdapterStatus, Error> {
    known(shared, profile, platform)?;
    let store = shared.store(profile).ok_or_else(|| unserved(profile))?;
    if store.resume_adapter(platform)? {
        tracing::info!(profile, platform, "adapter resumed");
    }
    match adapters::slot_of(shared, profile, platform) {
        Some(slot) if slot.state() != AdapterState::Fatal => slot.kick.notify_one(),
        _ => {
            adapters::forget(shared, profile, platform).await;
            adapters::start_profile(shared, profile);
        }
    }
    Ok(settled(shared, profile, platform).await)
}

/// The adapter's status once it left `connecting`/`retrying`, or as it is
/// after [`RESUME_REPORT_WAIT`].
async fn settled(shared: &Shared, profile: &str, platform: &str) -> AdapterStatus {
    let deadline = tokio::time::Instant::now() + RESUME_REPORT_WAIT;
    loop {
        let status = statuses_of(shared, profile)
            .into_iter()
            .find(|s| s.platform == platform);
        let done = status.as_ref().is_some_and(|s| {
            matches!(
                s.state,
                AdapterState::Connected | AdapterState::Degraded | AdapterState::Fatal
            )
        });
        if done || tokio::time::Instant::now() >= deadline {
            return status.unwrap_or_else(|| AdapterStatus {
                profile: profile.to_owned(),
                platform: platform.to_owned(),
                state: AdapterState::Disabled,
                code: None,
                message: None,
                attention: false,
                paused: false,
                descriptor: None,
            });
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn unserved(profile: &str) -> Error {
    Error::new(
        &codes::PROFILE_NOT_FOUND,
        format!("profile '{profile}' is not served"),
    )
}

impl Gateway {
    /// `/platform pause` from outside a chat (`prometheus gateway pause`).
    ///
    /// # Errors
    ///
    /// `gateway.no_adapter` for a platform the profile does not enable,
    /// store errors.
    pub fn pause_platform(
        &self,
        profile: &str,
        platform: &str,
        actor: &str,
    ) -> Result<bool, Error> {
        pause(&self.shared, profile, platform, actor)
    }

    /// `/platform resume` from outside a chat (`prometheus gateway
    /// resume`): the adapter's state once it settled.
    ///
    /// # Errors
    ///
    /// `gateway.no_adapter`, store errors.
    pub async fn resume_platform(
        &self,
        profile: &str,
        platform: &str,
    ) -> Result<AdapterStatus, Error> {
        resume(&self.shared, profile, platform).await
    }

    /// The id of role `name` as `platform`'s connected adapter of
    /// `profile` knows it (`None`: not connected, or no such role).
    pub async fn resolve_role(&self, profile: &str, platform: &str, name: &str) -> Option<String> {
        let slot = self.shared.adapters.ready(profile, platform)?;
        slot.adapter.resolve_role(name).await
    }
}

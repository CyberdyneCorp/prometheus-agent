//! Reconnect attention (spec: messaging-gateway, Session stall watchdog
//! and reconnect attention): an adapter outside `connected` for longer than
//! its profile's `gateway.reconnect_attention_secs` is flagged once per
//! outage. The flag rides on `adapter.state` (every attached client of the
//! profile sees it) and on `/platforms` and `gateway status`, and the
//! profile's other connected platforms get one notice in their home
//! channel. Reconnecting clears the flag; the next outage is a new one.

use std::sync::Arc;
use std::time::Duration;

use pmt_store::gateway::outbox::{NewOutbound, OutboundKind};
use tokio::time::Instant;
use tracing::Instrument as _;

use super::Shared;
use super::adapter::AdapterState;
use super::adapters;

/// How often outages are looked at.
const CHECK_EVERY: Duration = Duration::from_secs(1);

/// The notice the profile's other home channels get.
pub(crate) fn notice(
    platform: &str,
    outage: Duration,
    state: AdapterState,
    code: Option<&str>,
) -> String {
    let name = pmt_config::platforms::first_party(platform).map_or(platform, |p| p.display);
    let state = serde_json::to_value(state)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default();
    let why = code.map_or(state.clone(), |code| format!("{state}: {code}"));
    format!(
        "{name} has been unavailable for {} ({why}); I still answer here.",
        span(outage)
    )
}

/// `n min` from a minute on, `n s` below.
pub(crate) fn span(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs >= 60 {
        format!("{} min", secs / 60)
    } else {
        format!("{secs} s")
    }
}

/// Starts the attention check.
pub(crate) fn spawn(shared: &Arc<Shared>) {
    let weak = Arc::downgrade(shared);
    let stop = shared.stop.clone();
    tokio::spawn(
        async move {
            loop {
                tokio::select! {
                    () = stop.cancelled() => return,
                    () = tokio::time::sleep(CHECK_EVERY) => {}
                }
                let Some(shared) = weak.upgrade() else { return };
                check(&shared, Instant::now());
            }
        }
        .in_current_span(),
    );
}

fn check(shared: &Shared, now: Instant) {
    for ((profile, platform), slot) in adapters::all(shared) {
        let Some(since) = *slot.outside_since.lock().expect("slot poisoned") else {
            continue;
        };
        let status = slot.status();
        if status.attention || status.state == AdapterState::Disabled {
            continue;
        }
        let threshold = shared
            .agents
            .scope(&profile)
            .map_or(600, |s| s.config().gateway.reconnect_attention_secs.get());
        let outage = now.duration_since(since);
        if outage < Duration::from_secs(threshold) {
            continue;
        }
        slot.update(|s| s.attention = true);
        tracing::warn!(
            profile,
            platform,
            "the adapter has been unavailable for {}",
            span(outage)
        );
        let text = notice(&platform, outage, status.state, status.code.as_deref());
        let started = jiff::Timestamp::now().as_second()
            - i64::try_from(outage.as_secs()).unwrap_or(i64::MAX);
        tell_home_channels(shared, &profile, &platform, started, &text);
    }
}

/// One notice per outage to each other connected platform's home channel.
fn tell_home_channels(shared: &Shared, profile: &str, down: &str, started: i64, text: &str) {
    let Some(scope) = shared.agents.scope(profile) else {
        return;
    };
    let gateway = &scope.config().gateway;
    for ((owner, platform), _) in adapters::connected(shared) {
        if owner != profile || platform == down {
            continue;
        }
        let Some(home) = gateway.platform(&platform).home_channel else {
            continue;
        };
        let new = NewOutbound {
            idempotency_key: format!("attention:{down}:{started}:{platform}"),
            session_id: None,
            platform: platform.clone(),
            chat_id: home.chat_id.clone(),
            thread_id: home.thread_id.clone(),
            reply_to: None,
            edit_of: None,
            kind: OutboundKind::Warning,
            body: text.to_owned(),
            attachments: Vec::new(),
        };
        shared.deliver(profile, &new);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_eleven_minute_outage_is_named_in_minutes() {
        assert_eq!(
            notice(
                "discord",
                Duration::from_mins(11),
                AdapterState::Retrying,
                Some("network")
            ),
            "Discord has been unavailable for 11 min (retrying: network); I still answer here."
        );
        assert_eq!(span(Duration::from_secs(5)), "5 s");
    }
}

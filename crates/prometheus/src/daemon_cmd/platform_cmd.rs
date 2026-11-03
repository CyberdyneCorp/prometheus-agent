//! `prometheus gateway pause|resume <platform>` (spec: messaging-gateway,
//! Adapter operations from chat): the same as an admin's `/platform pause`
//! and `/platform resume`. With the daemon running it acts through the
//! control channel (a resume reconnects at once); without one the pause is
//! written to the profile's `state.db`, where the next daemon finds it.

use std::time::Duration;

use pmt_core::error::{Error, codes};
use pmt_rpc::control::ControlRequest;
use pmt_store::Store;
use serde::Serialize;

use super::{control_call, wait_running};
use crate::context::Context;
use crate::output::{document, print_raw};

/// What `gateway pause|resume --json` prints.
#[derive(Debug, Serialize)]
struct Outcome {
    profile: String,
    platform: String,
    paused: bool,
    /// The adapter's state when a daemon runs it.
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<String>,
}

/// Pauses (`pause = true`) or resumes `platform` of the selected profile.
///
/// # Errors
///
/// `gateway.no_adapter` for a platform the profile does not enable;
/// control channel and store errors.
pub(super) fn run(platform: &str, pause: bool, ctx: &Context<'_>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let platform = platform.to_lowercase();
    let profile = scope.name().to_string();
    if !scope.config().gateway.platform(&platform).enabled {
        return Err(Error::new(
            &codes::GATEWAY_NO_ADAPTER,
            format!("profile '{profile}' has no '{platform}' adapter"),
        ));
    }
    let home = ctx.home()?;
    let state = if let Some(record) = wait_running(home.root(), Duration::from_secs(2))? {
        let request = ControlRequest::GatewayPlatform {
            profile: profile.clone(),
            platform: platform.clone(),
            pause,
        };
        let response = control_call(&record, &request)?;
        if let Some(error) = &response.error {
            return Err(super::send_cmd::control_error(error));
        }
        response.status.and_then(|status| {
            status
                .adapters
                .into_iter()
                .find(|a| a.profile == profile && a.platform == platform)
                .map(|a| a.line())
        })
    } else {
        let store = Store::open(&scope)?;
        if pause {
            store.pause_adapter(&platform, "cli")?;
        } else {
            store.resume_adapter(&platform)?;
        }
        None
    };
    if ctx.json {
        let outcome = Outcome {
            profile,
            platform,
            paused: pause,
            state,
        };
        return document("gateway pause", &outcome);
    }
    let verb = if pause { "Paused" } else { "Resumed" };
    let text = match state {
        Some(line) => format!("{verb}. {line}\n"),
        None => {
            format!("{verb} {platform}; the daemon is not running, so it applies when it starts.\n")
        }
    };
    print_raw(&text)
}

//! `/handoff <platform>[:<chat_id>[:<thread_id>]]` in the line interface
//! and the TUI (spec: messaging-gateway, Session handoff to a messaging
//! platform): the running daemon's gateway moves the session to that chat
//! (see `pmt_gateway::messaging::handoff`), then this client detaches,
//! printing how to resume locally.

use std::path::Path;

use pmt_core::error::{Error, codes};
use pmt_rpc::control::{ControlRequest, call};

/// What `/handoff` without a destination answers.
const USAGE: &str = "usage: /handoff <platform>[:<chat_id>[:<thread_id>]]";

/// What the client prints as it detaches.
#[must_use]
pub fn detached(strings: &pmt_i18n::Strings, platform: &str, session: &str) -> String {
    pmt_i18n::t!(
        strings,
        "cli.handed_off",
        platform = platform,
        session = session
    )
}

/// Hands `session` of `profile` in `home` over to `target`; the line to
/// print as the client detaches.
///
/// # Errors
///
/// `cli.usage` without a target; `gateway.daemon_required` without a
/// running daemon; the daemon's refusal (`session.busy`,
/// `gateway.target_refused`, an unknown platform...).
pub async fn handoff(
    home: &Path,
    profile: &str,
    session: &str,
    target: &str,
    strings: std::sync::Arc<pmt_i18n::Strings>,
) -> Result<String, Error> {
    let target = target.trim();
    if target.is_empty() {
        return Err(Error::new(&codes::CLI_USAGE, USAGE));
    }
    let lookup = home.to_path_buf();
    let record = pmt_core::spawn::blocking(move || crate::daemon_client::running(&lookup))
        .await
        .map_err(|e| Error::new(&codes::RPC_INTERNAL, format!("daemon lookup failed: {e}")))??
        .ok_or_else(|| {
            Error::new(
                &codes::GATEWAY_DAEMON_REQUIRED,
                "/handoff needs the gateway daemon (start it with 'prometheus daemon start')",
            )
        })?;
    let request = ControlRequest::GatewayHandoff {
        profile: profile.to_owned(),
        session: session.to_owned(),
        target: target.to_owned(),
    };
    let response = call(&record.endpoint.control, &request).await?;
    if let Some(err) = &response.error {
        let code = pmt_core::error::ERROR_CODES
            .iter()
            .find(|c| c.code == err.code)
            .copied()
            .unwrap_or(&codes::GATEWAY_TARGET_REFUSED);
        return Err(Error::new(code, err.message.clone()));
    }
    let done = response
        .handoff
        .ok_or_else(|| Error::new(&codes::RPC_CONNECTION, "the daemon did not hand off"))?;
    Ok(detached(&strings, &done.platform, session))
}

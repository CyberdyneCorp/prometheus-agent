//! `prometheus gateway enroll` (spec: messaging-gateway, Relay
//! enrollment): the one-time token is exchanged with the relay connector
//! over TLS for a long-lived gateway credential, which goes only to the
//! launch profile's secrets store (`gateway.relay.token_secret`); the relay
//! URL, gateway id and wake URL go to `<home>/daemon.toml`. Neither the
//! token nor the credential is ever printed or logged, and a refused token
//! changes no file (exit 78).
//!
//! The exchange: `POST <connector-url>/v1/enroll` with `{"token",
//! "gateway_id"?, "wake_url"?}`; success is `{"gateway_id", "credential",
//! "relay_url"?}` (without `relay_url` the relay is `<connector-url>` with
//! `wss://` and the path `/v1/gateway`); a refusal is a 4xx with
//! `{"error": "already_used" | "expired" | ...}`.

use std::time::Duration;

use clap::Args;
use pmt_config::DaemonConfig;
use pmt_config::daemon_relay::{self, Enrollment, secure_url};
use pmt_core::error::{Error, codes};
use pmt_core::secret::SecretString;
use serde_json::{Value, json};

use crate::context::Context;

/// How long the connector has to answer.
const TIMEOUT: Duration = Duration::from_secs(30);

/// `prometheus gateway enroll` arguments.
#[derive(Debug, Args)]
pub struct EnrollArgs {
    /// The one-time enrollment token from the relay operator.
    #[arg(long)]
    pub token: String,
    /// The connector's HTTPS base URL.
    #[arg(long, value_name = "URL")]
    pub connector_url: String,
    /// The gateway id to ask for (the connector may assign one).
    #[arg(long, value_name = "ID")]
    pub gateway_id: Option<String>,
    /// An HTTPS URL the connector calls to wake this host when it scaled
    /// to zero.
    #[arg(long, value_name = "URL")]
    pub wake_url: Option<String>,
}

/// What the connector granted.
struct Granted {
    gateway_id: String,
    credential: SecretString,
    relay_url: String,
}

fn relay_failure(why: &str) -> Error {
    Error::new(&codes::GATEWAY_RELAY, format!("enrollment failed: {why}"))
}

/// The relay WebSocket URL of a connector base URL.
fn derived_relay_url(connector: &str) -> String {
    let base = connector.trim_end_matches('/');
    let socket = base
        .strip_prefix("https://")
        .map(|rest| format!("wss://{rest}"))
        .or_else(|| {
            base.strip_prefix("http://")
                .map(|rest| format!("ws://{rest}"))
        })
        .unwrap_or_else(|| base.to_owned());
    format!("{socket}/v1/gateway")
}

/// The refusal of a used or expired token.
fn rejected(reason: &str) -> Error {
    let words = match reason {
        "already_used" => "already used".to_owned(),
        "expired" => "expired".to_owned(),
        other => other.replace('_', " "),
    };
    Error::new(
        &codes::GATEWAY_ENROLL_REJECTED,
        format!("enrollment token rejected: {words}"),
    )
}

async fn exchange(client: reqwest::Client, args: &EnrollArgs) -> Result<Granted, Error> {
    let url = format!("{}/v1/enroll", args.connector_url.trim_end_matches('/'));
    let body = json!({
        "token": args.token,
        "gateway_id": args.gateway_id,
        "wake_url": args.wake_url,
    });
    let response = client
        .post(&url)
        .header("content-type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .map_err(|e| {
            relay_failure(&format!(
                "cannot reach {}: {}",
                args.connector_url,
                e.without_url()
            ))
        })?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    let answer: Value = serde_json::from_str(&text).map_err(|_| {
        relay_failure(&format!(
            "the connector answered HTTP {status} without JSON"
        ))
    })?;
    if status.is_client_error() {
        return Err(rejected(answer["error"].as_str().unwrap_or("refused")));
    }
    if !status.is_success() {
        return Err(relay_failure(&format!(
            "the connector answered HTTP {status}"
        )));
    }
    let field = |key: &str| {
        answer[key]
            .as_str()
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    };
    let (Some(gateway_id), Some(credential)) = (field("gateway_id"), field("credential")) else {
        return Err(relay_failure(
            "the answer carries no gateway id or credential",
        ));
    };
    let relay_url = field("relay_url").unwrap_or_else(|| derived_relay_url(&args.connector_url));
    Ok(Granted {
        gateway_id,
        credential: SecretString::new(credential),
        relay_url,
    })
}

/// Runs `prometheus gateway enroll`.
///
/// # Errors
///
/// `cli.usage` (2) for an insecure URL, `gateway.enroll_rejected` (78) for
/// a refused token, `gateway.relay` (69) when the connector cannot be
/// reached, store and config write errors.
pub(super) fn run(args: &EnrollArgs, ctx: &Context<'_>) -> Result<u8, Error> {
    for (flag, url) in [
        ("--connector-url", Some(&args.connector_url)),
        ("--wake-url", args.wake_url.as_ref()),
    ] {
        if let Some(url) = url
            && !secure_url(url, "https", "http")
        {
            return Err(Error::new(
                &codes::CLI_USAGE,
                format!("{flag} {url}: use https:// (http:// only to a loopback host)"),
            ));
        }
    }
    let home = ctx.home()?;
    let daemon = DaemonConfig::load(home.root())?;
    let scope = ctx.scope()?;
    let client = crate::update_cmd::outbound_policy(ctx);
    let client = pmt_net::client_builder(&client)?
        .timeout(TIMEOUT)
        .build()
        .map_err(|e| relay_failure(&e.to_string()))?;
    let granted = crate::model_cmd::block_on(exchange(client, args))??;
    let secret = &daemon.gateway.relay.token_secret.0;
    scope.secrets().store().set(secret, &granted.credential)?;
    daemon_relay::set_enrollment(
        home.root(),
        &Enrollment {
            url: &granted.relay_url,
            gateway_id: &granted.gateway_id,
            wake_url: args.wake_url.as_deref(),
        },
    )?;
    crate::output::print(&format!(
        "enrolled as gateway {}; credential stored as secret {secret}; relay settings written to {}\n\
         run 'prometheus gateway reload' (or start the daemon) to connect",
        granted.gateway_id,
        home.root().join(pmt_config::daemon::FILE_NAME).display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_relay_url_follows_the_connector() {
        assert_eq!(
            derived_relay_url("https://relay.example/"),
            "wss://relay.example/v1/gateway"
        );
        assert_eq!(
            derived_relay_url("http://127.0.0.1:9"),
            "ws://127.0.0.1:9/v1/gateway"
        );
    }

    #[test]
    fn a_used_token_is_named() {
        assert_eq!(
            rejected("already_used").message(),
            "enrollment token rejected: already used"
        );
        assert_eq!(rejected("already_used").exit_code(), 78);
    }
}

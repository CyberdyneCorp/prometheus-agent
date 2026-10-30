//! The device-code sign-in (spec: model-providers, Device-code and browser
//! PKCE sign-in flows): print the verification URL and user code, poll at
//! the server's interval, tolerate up to [`MAX_TRANSPORT_ERRORS`]
//! consecutive transport errors while polling, retry the start request and
//! the token exchange up to [`token::RETRIES`] times, and fail with
//! `auth.login_expired` when the code expires. Two protocols share the
//! loop: RFC 8628 and OpenAI's (a user code, then an authorization code
//! exchanged with PKCE).

use std::time::Duration;

use jiff::Timestamp;
use pmt_core::error::{Error, codes};
use serde_json::Value;
use tokio::time::Instant;

use super::SignInUi;
use super::spec::{DeviceProtocol, OAuthSpec};
use super::token::{self, Exchange, Reply, TokenSet};

/// Consecutive poll transport errors tolerated.
pub const MAX_TRANSPORT_ERRORS: u32 = 6;

/// RFC 8628 default poll interval.
const DEFAULT_INTERVAL: Duration = Duration::from_secs(5);
/// Added to the interval on `slow_down`.
const SLOW_DOWN: Duration = Duration::from_secs(5);
/// Lifetime of a code whose server names none (OpenAI's: 15 minutes).
const DEFAULT_LIFETIME: Duration = Duration::from_mins(15);

const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// A started device sign-in.
#[derive(Debug)]
struct Started {
    /// RFC 8628 `device_code` or OpenAI `device_auth_id`.
    handle: String,
    user_code: String,
    verification: String,
    interval: Duration,
    expires: Instant,
}

/// One poll's outcome.
enum Poll {
    Pending,
    SlowDown,
    Done(TokenSet),
    /// OpenAI: approved; the reply carries a code to exchange.
    Approved,
    Expired,
    Failed(String),
}

/// Runs a device-code sign-in to the end.
///
/// # Errors
///
/// `auth.login_expired`, `auth.login_failed`, `auth.signin_unavailable`
/// (no device endpoint).
pub async fn run(
    client: &reqwest::Client,
    spec: &OAuthSpec,
    ui: &dyn SignInUi,
) -> Result<TokenSet, Error> {
    let started = start(client, spec).await?;
    ui.say(&format!(
        "To sign in to {}, open {} and enter the code {}",
        spec.provider, started.verification, started.user_code
    ));
    ui.say("Waiting for the sign-in to be approved...");
    let mut interval = started.interval;
    let mut transport_errors = 0;
    loop {
        tokio::time::sleep(interval).await;
        if Instant::now() >= started.expires {
            return Err(expired(&spec.provider));
        }
        let reply = match poll_once(client, spec, &started).await {
            Ok(reply) => reply,
            Err(why) => {
                transport_errors += 1;
                if transport_errors > MAX_TRANSPORT_ERRORS {
                    return Err(failed(&format!(
                        "polling the sign-in failed {transport_errors} times in a row: {why}"
                    )));
                }
                continue;
            }
        };
        transport_errors = 0;
        match judge(spec, &reply) {
            Poll::Pending => {}
            Poll::SlowDown => interval += SLOW_DOWN,
            Poll::Done(tokens) => return Ok(tokens),
            Poll::Approved => return exchange_openai(client, spec, &reply).await,
            Poll::Expired => return Err(expired(&spec.provider)),
            Poll::Failed(why) => return Err(failed(&why)),
        }
    }
}

async fn start(client: &reqwest::Client, spec: &OAuthSpec) -> Result<Started, Error> {
    let url = spec.device_authorization_url.as_deref().ok_or_else(|| {
        Error::new(
            &codes::AUTH_SIGNIN_UNAVAILABLE,
            format!(
                "provider '{}' has no device-code sign-in endpoint",
                spec.provider
            ),
        )
    })?;
    let scope = spec.scopes.join(" ");
    let reply = match spec.device_protocol {
        DeviceProtocol::Rfc8628 => {
            let fields = [
                ("client_id", spec.client_id.as_str()),
                ("scope", scope.as_str()),
            ];
            token::post(client, url, &fields, false).await?
        }
        DeviceProtocol::OpenAi => {
            token::post(client, url, &[("client_id", spec.client_id.as_str())], true).await?
        }
    };
    if !(200..300).contains(&reply.status) {
        return Err(failed(&format!(
            "the device-code request was refused ({})",
            reply.describe()
        )));
    }
    started(spec, &reply)
}

fn started(spec: &OAuthSpec, reply: &Reply) -> Result<Started, Error> {
    let incomplete = || failed("the device-code response lacks a code");
    let (handle_field, uri) = match spec.device_protocol {
        DeviceProtocol::Rfc8628 => (
            "device_code",
            reply
                .field("verification_uri_complete")
                .or_else(|| reply.field("verification_uri")),
        ),
        DeviceProtocol::OpenAi => ("device_auth_id", None),
    };
    let handle = reply.field(handle_field).ok_or_else(incomplete)?;
    let user_code = reply.field("user_code").ok_or_else(incomplete)?;
    let verification = uri
        .map(str::to_owned)
        .or_else(|| spec.verification_url.clone())
        .ok_or_else(|| failed("the device-code response names no verification URL"))?;
    let seconds = |name: &str| {
        let value = reply.body.get(name)?;
        value
            .as_u64()
            .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
            .map(Duration::from_secs)
    };
    Ok(Started {
        handle: handle.to_owned(),
        user_code: user_code.to_owned(),
        verification,
        interval: seconds("interval").unwrap_or(DEFAULT_INTERVAL),
        expires: Instant::now() + seconds("expires_in").unwrap_or(DEFAULT_LIFETIME),
    })
}

async fn poll_once(
    client: &reqwest::Client,
    spec: &OAuthSpec,
    started: &Started,
) -> Result<Reply, String> {
    match spec.device_protocol {
        DeviceProtocol::Rfc8628 => {
            let fields = [
                ("grant_type", DEVICE_GRANT),
                ("device_code", started.handle.as_str()),
                ("client_id", spec.client_id.as_str()),
            ];
            token::post_once(client, &spec.token_url, &fields, false).await
        }
        DeviceProtocol::OpenAi => {
            let url = spec.device_poll_url.as_deref().unwrap_or(&spec.token_url);
            let fields = [
                ("device_auth_id", started.handle.as_str()),
                ("user_code", started.user_code.as_str()),
            ];
            token::post_once(client, url, &fields, true).await
        }
    }
}

fn judge(spec: &OAuthSpec, reply: &Reply) -> Poll {
    if reply.status >= 500 {
        return Poll::Pending;
    }
    match spec.device_protocol {
        DeviceProtocol::Rfc8628 if reply.status == 200 => {
            match TokenSet::from_reply(reply, Timestamp::now()) {
                Ok(tokens) => Poll::Done(tokens),
                Err(err) => Poll::Failed(err.message().to_owned()),
            }
        }
        DeviceProtocol::Rfc8628 => match reply.field("error") {
            Some("authorization_pending") => Poll::Pending,
            Some("slow_down") => Poll::SlowDown,
            Some("expired_token") => Poll::Expired,
            _ => Poll::Failed(format!("the sign-in was refused ({})", reply.describe())),
        },
        // OpenAI: 403/404 while pending; 200 carries the code.
        DeviceProtocol::OpenAi => match reply.status {
            200 => Poll::Approved,
            403 | 404 => Poll::Pending,
            410 => Poll::Expired,
            _ => Poll::Failed(format!("the sign-in was refused ({})", reply.describe())),
        },
    }
}

async fn exchange_openai(
    client: &reqwest::Client,
    spec: &OAuthSpec,
    reply: &Reply,
) -> Result<TokenSet, Error> {
    let field = |name: &str| reply.body.get(name).and_then(Value::as_str);
    let (Some(code), Some(verifier)) = (field("authorization_code"), field("code_verifier")) else {
        return Err(failed(
            "the approved sign-in carries no authorization code or verifier",
        ));
    };
    let redirect_uri = spec.device_redirect_uri.as_deref().unwrap_or_default();
    let exchange = Exchange {
        code,
        verifier,
        redirect_uri,
        state: None,
    };
    token::exchange_code(client, spec, &exchange).await
}

fn expired(provider: &str) -> Error {
    Error::new(
        &codes::AUTH_LOGIN_EXPIRED,
        format!("the device code for {provider} expired before the sign-in was approved"),
    )
    .with_hint(format!("prometheus auth add {provider}"))
}

fn failed(message: &str) -> Error {
    Error::new(&codes::AUTH_LOGIN_FAILED, message)
}

//! The interactive pairing flows the dashboard's channel setup forms host
//! (spec: web-dashboard, Channel setup forms; desktop-app, Telegram quick
//! setup by QR):
//!
//! - `WhatsApp`: the bridge links the profile as a linked device; each QR
//!   code it reports is sent as an SVG image until the phone links, at
//!   most 120 seconds, then the adapter restarts with the new session.
//! - Telegram Create with QR: the onboarding service
//!   (`gateway.platforms.telegram.options.setup_service_url`) is asked for
//!   a pairing (`POST /v1/telegram/pairings`), its QR code and deep link
//!   are shown, and the pairing is polled every 2 seconds for at most 180;
//!   a well-formed bot token is stored as `TELEGRAM_BOT_TOKEN`, the
//!   returned owner becomes the only allowlisted user and only the
//!   Telegram adapter restarts. Expiry and cancellation save nothing.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use pmt_config::ProfileScope;
use pmt_config::platforms::PairingFlow;
use pmt_core::error::{Error, codes};
use pmt_core::secret::SecretString;
use pmt_gateway::messaging::Gateway;
use serde_json::{Value, json};

use crate::daemon_cmd::whatsapp_cmd::{self, Shown};

/// How often the onboarding service is polled.
const POLL_EVERY: Duration = Duration::from_secs(2);
/// How long Create with QR waits for the user.
const TELEGRAM_WINDOW: Duration = Duration::from_mins(3);
/// The bot name proposed to the onboarding service.
const BOT_NAME: &str = "Prometheus Agent";
/// The message of an expired Telegram setup.
pub(super) const TELEGRAM_TIMED_OUT: &str = "Telegram setup timed out; nothing was saved";

/// `code` as an SVG QR code image (a `data:` URI).
fn qr_image(code: &str) -> Result<String, Error> {
    let qr = qrcode::QrCode::new(code.as_bytes()).map_err(|e| {
        Error::new(
            &codes::GATEWAY_DELIVERY_FAILED,
            format!("the QR data cannot be drawn: {e}"),
        )
    })?;
    let svg = qr
        .render::<qrcode::render::svg::Color<'_>>()
        .min_dimensions(240, 240)
        .build();
    Ok(format!(
        "data:image/svg+xml;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(svg)
    ))
}

fn cancelled() -> Error {
    Error::new(
        &codes::GATEWAY_PAIRING_TIMEOUT,
        "pairing was cancelled; nothing was saved",
    )
}

/// Runs `flow` for `scope`'s `platform`.
pub(super) async fn run(
    scope: ProfileScope,
    gateway: Option<Gateway>,
    flow: PairingFlow,
    sink: pmt_web::PairingSink,
) -> Result<Value, Error> {
    let done = match flow {
        PairingFlow::WhatsappQr => whatsapp(&scope, &sink).await?,
        PairingFlow::TelegramCreate => telegram(&scope, &sink).await?,
    };
    let platform = match flow {
        PairingFlow::WhatsappQr => "whatsapp",
        PairingFlow::TelegramCreate => "telegram",
    };
    let mut done = done;
    if let Some(gateway) = gateway {
        let status = gateway
            .restart_platform(scope.name().as_str(), platform)
            .await?;
        done["adapter_state"] = serde_json::to_value(status.state).unwrap_or_default();
    }
    Ok(done)
}

async fn whatsapp(scope: &ProfileScope, sink: &pmt_web::PairingSink) -> Result<Value, Error> {
    let root = scope.home().to_path_buf();
    let policy = crate::runtimes_cmd::browser::outbound(&scope.config().network);
    let progress: pmt_update::runtimes::install::Progress =
        Arc::new(|line| tracing::info!("whatsapp bridge: {line}"));
    let bridge =
        pmt_core::spawn::blocking(move || whatsapp_cmd::bridge_at(&root, &policy, &progress))
            .await
            .map_err(|e| Error::new(&codes::RUNTIME_IO, format!("bridge install failed: {e}")))??;
    let command = whatsapp_cmd::link_command(scope, &bridge, None)?;
    let mut show = |shown: Shown<'_>| {
        match shown {
            Shown::Qr(data) => sink.emit(json!({"state": "qr", "image": qr_image(data)?})),
            Shown::Code(code) => sink.emit(json!({"state": "code", "code": code})),
        }
        Ok(())
    };
    let account = whatsapp_cmd::pair(
        command,
        whatsapp_cmd::pair_window(),
        sink.cancel(),
        &mut show,
    )
    .await?;
    Ok(json!({"state": "linked", "account": account}))
}

/// A pairing the onboarding service created.
struct Pairing {
    id: String,
    poll_token: String,
    deep_link: String,
    qr: String,
    username: String,
}

/// What a finished pairing returned.
struct Created {
    token: String,
    bot_username: Option<String>,
    owner: String,
}

fn service_error(message: impl Into<String>) -> Error {
    Error::new(&codes::GATEWAY_DELIVERY_FAILED, message)
}

/// Telegram's bot token shape: `<digits>:<30+ of [A-Za-z0-9_-]>`.
fn valid_token(token: &str) -> bool {
    token.split_once(':').is_some_and(|(id, secret)| {
        !id.is_empty()
            && id.bytes().all(|b| b.is_ascii_digit())
            && secret.len() >= 30
            && secret
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    })
}

/// How long Create with QR waits (shortened only in debug builds' tests).
fn telegram_window() -> Duration {
    #[cfg(debug_assertions)]
    if let Some(secs) = std::env::var("PMT_TEST_TELEGRAM_SETUP_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
    {
        return Duration::from_secs(secs);
    }
    TELEGRAM_WINDOW
}

async fn create_pairing(client: &reqwest::Client, base: &str) -> Result<Pairing, Error> {
    let answer = client
        .post(format!("{base}/v1/telegram/pairings"))
        .json(&json!({"bot_name": BOT_NAME}))
        .send()
        .await
        .map_err(|e| {
            service_error(format!(
                "the Telegram onboarding service is unreachable: {e}"
            ))
        })?;
    if !answer.status().is_success() {
        return Err(service_error(format!(
            "the Telegram onboarding service refused the pairing ({})",
            answer.status()
        )));
    }
    let body: Value = answer
        .json()
        .await
        .map_err(|e| service_error(format!("bad answer from the onboarding service: {e}")))?;
    let field = |key: &str| {
        body[key]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    let (Some(id), Some(poll_token), Some(deep_link)) =
        (field("pairing_id"), field("poll_token"), field("deep_link"))
    else {
        return Err(service_error(
            "the onboarding service's pairing lacks pairing_id, poll_token or deep_link",
        ));
    };
    Ok(Pairing {
        qr: field("qr_payload").unwrap_or_else(|| deep_link.clone()),
        username: field("suggested_username").unwrap_or_default(),
        id,
        poll_token,
        deep_link,
    })
}

/// One poll: the created bot once the pairing is ready.
async fn poll_once(client: &reqwest::Client, base: &str, pairing: &Pairing) -> Option<Created> {
    let answer = client
        .get(format!("{base}/v1/telegram/pairings/{}", pairing.id))
        .bearer_auth(&pairing.poll_token)
        .send()
        .await
        .ok()?;
    if !answer.status().is_success() {
        return None;
    }
    let body: Value = answer.json().await.ok()?;
    let token = body["token"].as_str().filter(|t| valid_token(t))?;
    if body["status"] != "ready" {
        return None;
    }
    let owner = match &body["owner_user_id"] {
        Value::Number(n) => n.as_u64().filter(|n| *n > 0).map(|n| n.to_string()),
        Value::String(s) if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) => {
            Some(s.clone())
        }
        _ => None,
    }?;
    Some(Created {
        token: token.to_owned(),
        bot_username: body["bot_username"].as_str().map(str::to_owned),
        owner,
    })
}

async fn telegram(scope: &ProfileScope, sink: &pmt_web::PairingSink) -> Result<Value, Error> {
    let options: pmt_config::platforms::TelegramOptions = pmt_config::platforms::options(
        "telegram",
        &scope.config().gateway.platform("telegram").options,
    )?;
    let base = options.setup_service_url.trim_end_matches('/').to_owned();
    let policy = crate::runtimes_cmd::browser::outbound(&scope.config().network);
    let client = pmt_net::http::client_builder(&policy)?
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| service_error(format!("cannot build a client: {e}")))?;
    let pairing = create_pairing(&client, &base).await?;
    sink.emit(json!({
        "state": "qr",
        "image": qr_image(&pairing.qr)?,
        "deep_link": pairing.deep_link,
        "username": pairing.username,
    }));
    let deadline = tokio::time::Instant::now() + telegram_window();
    let created = loop {
        if let Some(created) = poll_once(&client, &base, &pairing).await {
            break created;
        }
        if tokio::time::Instant::now() + POLL_EVERY > deadline {
            return Err(Error::new(
                &codes::GATEWAY_PAIRING_TIMEOUT,
                TELEGRAM_TIMED_OUT,
            ));
        }
        tokio::select! {
            () = sink.cancel().cancelled() => return Err(cancelled()),
            () = tokio::time::sleep(POLL_EVERY) => {}
        }
    };
    // A result that arrives after the user closed the flow is discarded.
    if sink.cancel().is_cancelled() {
        return Err(cancelled());
    }
    save_telegram(scope, &created)?;
    Ok(json!({
        "state": "linked",
        "account": created.bot_username,
        "owner_user_id": created.owner,
    }))
}

/// Stores the token, then makes the owner the only allowlisted user and
/// enables the adapter in one validated write; the token is removed again
/// when that write is refused.
fn save_telegram(scope: &ProfileScope, created: &Created) -> Result<(), Error> {
    const SECRET: &str = "TELEGRAM_BOT_TOKEN";
    pmt_config::managed::check_secret_writable(SECRET)?;
    let store = scope.secrets().store();
    store.set(SECRET, &SecretString::new(created.token.as_str()))?;
    let allow = serde_json::to_string(&[created.owner.as_str()]).unwrap_or_default();
    let written = scope.profile().config_file().set_all(&[
        ("gateway.platforms.telegram.enabled", "true"),
        ("gateway.platforms.telegram.allow_users", allow.as_str()),
    ]);
    if let Err(err) = written {
        let _ = store.remove(SECRET);
        return Err(err);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::valid_token;

    #[test]
    fn only_telegram_shaped_tokens_are_accepted() {
        assert!(valid_token("123456:ABCDEFGHIJKLMNOPQRSTUVWXYZabcd_-"));
        for bad in [
            "",
            "123456",
            ":ABCDEFGHIJKLMNOPQRSTUVWXYZabcdef",
            "12a:ABCDEFGHIJKLMNOPQRSTUVWXYZabcdef",
            "123456:short",
            "123456:ABCDEFGHIJKLMNOPQRSTUVWXYZabcd!!",
        ] {
            assert!(!valid_token(bad), "{bad}");
        }
    }
}

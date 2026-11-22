//! `prometheus gateway setup weixin` (spec: platform-adapters, Weixin
//! personal WeChat adapter): logs a personal WeChat account in to the
//! iLink bot API by QR. The login QR (`ilink/bot/get_bot_qrcode`) is
//! scanned with WeChat, its status long-polled
//! (`ilink/bot/get_qrcode_status`: `wait`, `scaned`, `scaned_but_redirect`
//! to another host, `expired` — refreshed up to three times — or
//! `confirmed`); the confirmed account's token goes to the secrets store
//! as `WEIXIN_TOKEN` and its id to `[gateway.platforms.weixin]` option
//! `account_id` (and `base_url` when iLink names another host). The token
//! is never printed. Option `base_url` points the login elsewhere.

use std::time::Duration;

use pmt_core::error::{Error, codes};
use pmt_core::secret::SecretString;
use serde_json::Value;

const PLATFORM: &str = "weixin";
const TOKEN: &str = "WEIXIN_TOKEN";
const DEFAULT_BASE: &str = "https://ilinkai.weixin.qq.com";
const MAX_REFRESHES: u32 = 3;
/// How long the login waits for a scan.
const LOGIN_WINDOW: Duration = Duration::from_mins(8);

fn failed(what: &str, why: impl std::fmt::Display) -> Error {
    Error::new(&codes::NET_REQUEST, format!("Weixin {what}: {why}"))
}

async fn get(client: &reqwest::Client, url: &str) -> Result<Value, Error> {
    let response = client
        .get(url)
        .header("iLink-App-Id", "bot")
        .header("iLink-App-ClientVersion", "131584")
        // The status call is a long poll of up to 35 seconds.
        .timeout(Duration::from_secs(40))
        .send()
        .await
        .map_err(|e| failed("login", e))?;
    let status = response.status();
    if !status.is_success() {
        return Err(failed("login", format!("HTTP {status}")));
    }
    response.json().await.map_err(|e| failed("login", e))
}

/// A fresh login QR: its code and the URL WeChat scans.
async fn qr(client: &reqwest::Client, base: &str) -> Result<String, Error> {
    let answer = get(
        client,
        &format!("{base}/ilink/bot/get_bot_qrcode?bot_type=3"),
    )
    .await?;
    let code = answer["qrcode"]
        .as_str()
        .filter(|c| !c.is_empty())
        .ok_or_else(|| failed("login", "no QR code in the answer"))?;
    // WeChat scans the page URL, not the bare code.
    let url = answer["qrcode_img_content"]
        .as_str()
        .filter(|u| !u.is_empty())
        .unwrap_or(code);
    let drawn = super::whatsapp_cmd::qr_text(url)?;
    crate::output::print_raw(&format!(
        "Scan this QR code with WeChat (or open {url}):\n{drawn}\n"
    ))?;
    Ok(code.to_owned())
}

/// The confirmed account: id, token, API base.
struct Account {
    id: String,
    token: String,
    base: String,
}

async fn login(client: &reqwest::Client, base: &str) -> Result<Account, Error> {
    let mut code = qr(client, base).await?;
    let mut host = base.to_owned();
    let (mut refreshes, mut scanned) = (0, false);
    let deadline = std::time::Instant::now() + LOGIN_WINDOW;
    while std::time::Instant::now() < deadline {
        let url = format!("{host}/ilink/bot/get_qrcode_status?qrcode={code}");
        let Ok(answer) = get(client, &url).await else {
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        };
        match answer["status"].as_str().unwrap_or("wait") {
            "scaned" if !scanned => {
                scanned = true;
                crate::output::print_raw("Scanned; confirm the login in WeChat.\n")?;
            }
            "scaned_but_redirect" => {
                if let Some(redirect) = answer["redirect_host"].as_str().filter(|h| !h.is_empty()) {
                    host = format!("https://{redirect}");
                }
            }
            "expired" => {
                refreshes += 1;
                if refreshes > MAX_REFRESHES {
                    break;
                }
                crate::output::print_raw("The QR code expired; here is a new one.\n")?;
                code = qr(client, base).await?;
                host = base.to_owned();
            }
            "confirmed" => {
                let text = |k: &str| answer[k].as_str().unwrap_or_default().to_owned();
                let account = Account {
                    id: text("ilink_bot_id"),
                    token: text("bot_token"),
                    base: Some(text("baseurl"))
                        .filter(|b| !b.is_empty())
                        .unwrap_or_else(|| base.to_owned()),
                };
                if account.id.is_empty() || account.token.is_empty() {
                    return Err(failed(
                        "login",
                        "WeChat confirmed without an account id and token",
                    ));
                }
                return Ok(account);
            }
            _ => {}
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    Err(Error::new(
        &codes::AUTH_LOGIN_EXPIRED,
        "the WeChat login was not confirmed in time; run `prometheus gateway setup weixin` again",
    ))
}

/// Runs the login and stores what it yields.
///
/// # Errors
///
/// The login failed or expired; the secrets store or the config file.
pub(super) async fn setup(scope: &pmt_config::ProfileScope) -> Result<String, Error> {
    let base = scope
        .config()
        .gateway
        .platforms
        .get(PLATFORM)
        .and_then(|p| {
            p.options
                .get("base_url")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| DEFAULT_BASE.to_owned());
    let base = base.trim_end_matches('/').to_owned();
    let client = pmt_net::http::client_builder(&pmt_net::OutboundPolicy::default())?
        .build()
        .map_err(|e| Error::new(&codes::NET_CLIENT_INIT, e.to_string()))?;
    let account = login(&client, &base).await?;
    scope
        .secrets()
        .store()
        .set(TOKEN, &SecretString::new(&account.token))?;
    let key = |k: &str| format!("gateway.platforms.{PLATFORM}.options.{k}");
    let mut values = vec![(
        key("account_id"),
        toml_edit::Value::from(account.id.as_str()),
    )];
    let account_base = account.base.trim_end_matches('/');
    if account_base != base {
        values.push((key("base_url"), toml_edit::Value::from(account_base)));
    }
    scope.profile().config_file().set_values(&values)?;
    Ok(format!(
        "Logged in WeChat account {}; its token is stored as {TOKEN}.\n\
         Enable it with: prometheus config set gateway.platforms.{PLATFORM}.enabled true\n",
        account.id
    ))
}

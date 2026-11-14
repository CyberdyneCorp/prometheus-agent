//! `prometheus webapp` (spec: web-dashboard, Desktop workspace in the
//! browser; Webapp sign-out and insecure-context notice): the desktop
//! workspace served under `/app` by the dashboard listener when
//! `webapp.enabled = true`. Without flags it ensures the daemon and opens
//! a one-use sign-in link (60 s) landing on `/app` (printed with
//! `--no-open` or when no browser starts); `--status` says whether and
//! where it is served; `--sign-out` ends every browser sign-in to the
//! listener (paired desktop devices stay signed in). Plain HTTP on a
//! non-loopback address gets a warning: browsers keep the microphone,
//! clipboard reading, notifications and screen viewing from it.

use std::time::Duration;

use pmt_config::daemon_dashboard::{WebappConfig, is_loopback_host};
use pmt_core::error::{Error, codes};
use pmt_providers::auth::SignInUi as _;
use pmt_rpc::control::DashboardAction;
use serde_json::json;

use crate::context::Context;
use crate::daemon_cmd::{running_or_start, wait_running};
use crate::dashboard_cmd::ask;
use crate::output::{document, print_raw};

/// `prometheus webapp` arguments.
#[derive(Debug, Clone, clap::Args)]
pub struct WebappArgs {
    /// Print the sign-in link instead of opening a browser.
    #[arg(long)]
    pub no_open: bool,
    /// Print whether and where the webapp is served (exit 69 when not).
    #[arg(long, conflicts_with_all = ["no_open", "sign_out"])]
    pub status: bool,
    /// Sign every browser out of this listener (cookies, CSRF tokens, open
    /// sockets); paired desktop devices stay signed in.
    #[arg(long, conflicts_with = "no_open")]
    pub sign_out: bool,
}

/// The path the webapp is served under.
const APP: &str = "app";

fn disabled() -> Error {
    Error::new(
        &codes::WEBAPP_DISABLED,
        "webapp disabled (set webapp.enabled = true)",
    )
}

/// `url` with its origin replaced by `webapp.public_url` when set.
fn public(url: &str, webapp: &WebappConfig) -> String {
    if webapp.public_url.is_empty() {
        return url.to_owned();
    }
    let path = url
        .split_once("://")
        .and_then(|(_, rest)| rest.find('/').map(|i| &rest[i..]))
        .unwrap_or("/");
    format!("{}{path}", webapp.public_url.trim_end_matches('/'))
}

/// The insecure-context warning for `url`, if any.
fn insecure(url: &str) -> Option<String> {
    let rest = url.strip_prefix("http://")?;
    let host = rest.split('/').next().unwrap_or_default();
    let name = host.rsplit_once(':').map_or(host, |(name, _)| name);
    (!is_loopback_host(name)).then(|| {
        format!(
            "warning: {url} is plain HTTP on a non-loopback host: browsers keep the microphone, \
             clipboard reading, notifications and screen viewing unavailable there; serve it \
             over HTTPS (dashboard.tls or a TLS proxy)"
        )
    })
}

/// Runs `prometheus webapp`.
///
/// # Errors
///
/// `webapp.disabled` (69) when `webapp.enabled` is false,
/// `dashboard.not_running` (69) for `--status` without a listener,
/// daemon and listener errors.
pub fn run(args: &WebappArgs, ctx: &Context<'_>) -> Result<u8, Error> {
    let home = ctx.home()?;
    let config = pmt_config::DaemonConfig::load(home.root())?;
    if args.sign_out {
        return sign_out(home.root(), ctx.json);
    }
    if !config.webapp.enabled {
        return Err(disabled());
    }
    if args.status {
        let record = wait_running(home.root(), Duration::from_secs(2))?
            .ok_or_else(|| not_served("the daemon is not running"))?;
        let state = ask(&record, DashboardAction::Status)
            .map_err(|_| not_served("the dashboard listener is not running"))?;
        let url = public(&format!("{}{APP}", state.url), &config.webapp);
        return show(ctx.json, &url, None, false);
    }
    let (record, _) = running_or_start(home.root(), ctx.profile)?;
    let action = DashboardAction::Open {
        host: None,
        port: None,
        next: Some(format!("/{APP}")),
    };
    let state = ask(&record, action)?;
    let url = public(&format!("{}{APP}", state.url), &config.webapp);
    let login = public(
        state.login_url.as_deref().unwrap_or(&state.url),
        &config.webapp,
    );
    let opened =
        !args.no_open && (crate::auth_cmd::TerminalUi { launch: ctx.launch }).open_browser(&login);
    show(ctx.json, &url, Some(&login), opened)
}

fn not_served(why: &str) -> Error {
    Error::new(
        &codes::DASHBOARD_NOT_RUNNING,
        format!("webapp not served: {why}"),
    )
    .with_hint("start it with 'prometheus webapp'")
}

fn show(as_json: bool, url: &str, login: Option<&str>, opened: bool) -> Result<u8, Error> {
    let warning = insecure(url);
    if as_json {
        return document(
            "webapp",
            &json!({"url": url, "login_url": login, "opened": opened, "warning": warning}),
        );
    }
    if let Some(warning) = &warning {
        eprintln!("{warning}");
    }
    let text = match login {
        None => format!("webapp at {url}\n"),
        Some(_) if opened => {
            format!("Webapp: {url}\nOpened a one-time sign-in link in your browser.\n")
        }
        Some(login) => {
            format!("Webapp: {url}\nOpen this one-time sign-in link (valid 60 s): {login}\n")
        }
    };
    print_raw(&text)
}

fn sign_out(home: &std::path::Path, as_json: bool) -> Result<u8, Error> {
    let ended = match wait_running(home, Duration::from_secs(2))? {
        Some(record) => ask(&record, DashboardAction::SignOut)?
            .signed_out
            .unwrap_or_default(),
        None => 0,
    };
    if as_json {
        return document("webapp", &json!({"signed_out": ended}));
    }
    print_raw(&format!(
        "Signed out {ended} browser session(s); paired devices stay signed in.\n"
    ))
}

#[cfg(test)]
mod tests {
    use pmt_config::daemon_dashboard::WebappConfig;

    use super::{insecure, public};

    #[test]
    fn plain_http_off_loopback_is_warned_about() {
        assert!(insecure("http://127.0.0.1:9120/app").is_none());
        assert!(insecure("https://dash.example.com/app").is_none());
        let warning = insecure("http://192.168.1.5:9120/app").unwrap();
        assert!(warning.contains("microphone"), "{warning}");
    }

    #[test]
    fn the_public_url_replaces_the_origin() {
        let webapp = WebappConfig {
            enabled: true,
            public_url: "https://dash.example.com".into(),
        };
        assert_eq!(
            public("http://127.0.0.1:9120/login?ticket=t&next=%2Fapp", &webapp),
            "https://dash.example.com/login?ticket=t&next=%2Fapp"
        );
    }
}

//! `prometheus dashboard` (spec: web-dashboard, Dashboard command and
//! listener defaults; Listener lifecycle within the daemon): ensures the
//! gateway daemon runs, asks it over its owner-only control channel to
//! open its dashboard listener, and opens the browser on a one-time login
//! link (printed instead with `--no-open` or when no browser starts).
//! `--status` prints the URL (exit 69 without a listener) and `--stop`
//! stops the listener while the daemon keeps running.

use std::fmt::Write as _;
use std::time::Duration;

use pmt_core::error::{Error, codes};
use pmt_providers::auth::SignInUi as _;
use pmt_rpc::control::{ControlRequest, DashboardAction, DashboardState};
use serde::Serialize;

use crate::context::Context;
use crate::daemon_cmd::{control_call, running_or_start, send_cmd::control_error, wait_running};
use crate::output::{document, print_raw};

/// `prometheus dashboard` arguments.
#[derive(Debug, Clone, clap::Args)]
pub struct DashboardArgs {
    /// Bind this address instead of `dashboard.host` (until the daemon
    /// stops). A non-loopback address needs authentication and TLS.
    #[arg(long)]
    pub host: Option<String>,
    /// Bind this port instead of `dashboard.port` (`0`: any free port).
    #[arg(long)]
    pub port: Option<u16>,
    /// Print the login link instead of opening a browser.
    #[arg(long)]
    pub no_open: bool,
    /// Print the running dashboard's URL (exit 69 when none runs).
    #[arg(long, conflicts_with_all = ["stop", "host", "port"])]
    pub status: bool,
    /// Stop the dashboard listener; the daemon keeps running.
    #[arg(long, conflicts_with_all = ["host", "port"])]
    pub stop: bool,
}

#[derive(Serialize)]
struct Shown<'a> {
    url: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    login_url: Option<&'a str>,
    started: bool,
    opened: bool,
}

fn not_running() -> Error {
    Error::new(
        &codes::DASHBOARD_NOT_RUNNING,
        "no dashboard listener is running",
    )
    .with_hint("start one with 'prometheus dashboard'")
}

/// One `dashboard` control request to the running daemon.
fn ask(
    record: &pmt_rpc::discovery::DaemonRecord,
    action: DashboardAction,
) -> Result<DashboardState, Error> {
    let response = control_call(record, &ControlRequest::Dashboard { action })?;
    if let Some(error) = &response.error {
        return Err(control_error(error));
    }
    response.dashboard.ok_or_else(not_running)
}

/// Runs `prometheus dashboard`.
///
/// # Errors
///
/// `dashboard.not_running` (69) for `--status`/`--stop` without a
/// listener, `dashboard.bind_refused` and `dashboard.port_in_use` (78),
/// daemon start and control channel errors.
pub fn run(args: &DashboardArgs, ctx: &Context<'_>) -> Result<u8, Error> {
    let home = ctx.home()?;
    if args.status || args.stop {
        let record = wait_running(home.root(), Duration::from_secs(2))?.ok_or_else(not_running)?;
        let action = if args.stop {
            DashboardAction::Stop
        } else {
            DashboardAction::Status
        };
        let state = ask(&record, action)?;
        if ctx.json {
            return document(
                "dashboard",
                &Shown {
                    url: &state.url,
                    login_url: None,
                    started: false,
                    opened: false,
                },
            );
        }
        let text = if args.stop {
            format!("Stopped the dashboard at {}\n", state.url)
        } else {
            format!("{}\n", state.url)
        };
        return print_raw(&text);
    }
    let (record, _) = running_or_start(home.root(), ctx.profile)?;
    let action = DashboardAction::Open {
        host: args.host.clone(),
        port: args.port,
    };
    let state = ask(&record, action)?;
    let login = state.login_url.as_deref().unwrap_or(&state.url);
    let opened =
        !args.no_open && (crate::auth_cmd::TerminalUi { launch: ctx.launch }).open_browser(login);
    if ctx.json {
        return document(
            "dashboard",
            &Shown {
                url: &state.url,
                login_url: state.login_url.as_deref(),
                started: state.started,
                opened,
            },
        );
    }
    let mut text = format!("Dashboard: {}\n", state.url);
    if opened {
        text.push_str("Opened a one-time login link in your browser.\n");
    } else {
        let _ = writeln!(text, "Open this one-time login link (valid 60 s): {login}");
    }
    print_raw(&text)
}

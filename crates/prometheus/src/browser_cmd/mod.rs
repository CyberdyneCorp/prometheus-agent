//! `prometheus browser status|close-profile` (spec: execution-environments,
//! Connecting to the user's browser; First-party cloud browser providers;
//! Real-profile browsing).
//!
//! `status` shows how the profile's browser sessions resolve (provider,
//! explicit endpoint, real-profile browsing) and the live sessions of
//! every process of the profile with their origin and provider.
//! `close-profile` ends the user's real browser holding the profile that
//! real-profile browsing copies, after confirmation.

use std::fmt::Write as _;

use clap::Subcommand;
use pmt_config::ProfileScope;
use pmt_core::error::Error;
use pmt_tools::browser::real_profile::{self, HostDetect};
use pmt_tools::browser::status::{Entry, Sessions};
use serde::Serialize;

use crate::context::Context;
use crate::output::{document, print_raw};

/// `prometheus browser` subcommands.
#[derive(Debug, Clone, Subcommand)]
pub enum BrowserCommand {
    /// Show the browser provider, explicit endpoint and live browser
    /// sessions with their provider.
    Status,
    /// End your real browser so real-profile browsing can copy its
    /// profile (asks first).
    CloseProfile {
        /// Do not ask for confirmation.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Serialize)]
struct Status {
    provider: String,
    cdp_url: Option<String>,
    use_real_profile: bool,
    headed: bool,
    sessions: Vec<Entry>,
}

/// Runs one `browser` subcommand.
///
/// # Errors
///
/// The profile cannot be opened, or stdout cannot be written.
pub fn run(command: &BrowserCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    match command {
        BrowserCommand::Status => status(&scope, ctx.json),
        BrowserCommand::CloseProfile { yes } => close_profile(ctx, *yes),
    }
}

fn status(scope: &ProfileScope, json: bool) -> Result<u8, Error> {
    let config = &scope.config().browser;
    let body = Status {
        provider: config.provider.clone(),
        cdp_url: config.cdp_url.clone().filter(|u| !u.trim().is_empty()),
        use_real_profile: config.use_real_profile,
        headed: config.headed,
        sessions: Sessions::new(&scope.path("cache/browser")).live(),
    };
    if json {
        return document("browser status", &body);
    }
    print_raw(&render(&body))
}

fn render(status: &Status) -> String {
    let mut out = format!("provider: {}\n", status.provider);
    if let Some(url) = &status.cdp_url {
        let _ = writeln!(out, "cdp_url: {url}");
    }
    let _ = writeln!(
        out,
        "real profile: {}\nheaded: {}",
        if status.use_real_profile { "on" } else { "off" },
        status.headed
    );
    if status.sessions.is_empty() {
        out.push_str("no live browser sessions\n");
        return out;
    }
    out.push_str("sessions:\n");
    for entry in &status.sessions {
        let _ = write!(out, "  {}  {}", entry.key, entry.origin);
        if let Some(provider) = &entry.provider {
            let _ = write!(out, "  provider={provider}");
        }
        if let Some(endpoint) = &entry.endpoint {
            let _ = write!(out, "  endpoint={endpoint}");
        }
        if let Some(pid) = entry.pid {
            let _ = write!(out, "  pid={pid}");
        }
        if let Some(profile) = &entry.profile {
            let _ = write!(out, "  profile={profile}");
        }
        if entry.fallback_from_cloud {
            out.push_str("  fallback_from_cloud");
        }
        out.push('\n');
    }
    out
}

fn close_profile(ctx: &Context<'_>, yes: bool) -> Result<u8, Error> {
    let launch = ctx.launch;
    let detect = HostDetect {
        user_home: launch.user_home().cloned().unwrap_or_default(),
        path_var: launch.inherited("PATH").map(ToOwned::to_owned),
    };
    let browser = match pmt_tools::browser::real_profile::Detect::default_browser(&detect) {
        Ok(browser) => browser,
        Err(why) => {
            eprintln!("error: {why}");
            return Ok(1);
        }
    };
    if !yes {
        let question = format!(
            "this ends {} ({}) and every window it has open; continue?",
            browser.name,
            browser.executable.display()
        );
        if !crate::prompt::stdin_is_tty() {
            eprintln!("error: closing the browser needs confirmation; rerun with --yes");
            return Ok(pmt_core::exit::APPROVAL_REQUIRED);
        }
        if !crate::prompt::confirm(&question)? {
            eprintln!("not closed");
            return Ok(1);
        }
    }
    match real_profile::close_profile(&detect) {
        Ok(ended) => print_raw(&format!("ended {ended} {} processes\n", browser.name)),
        Err(why) => {
            eprintln!("error: {why}");
            Ok(1)
        }
    }
}
